//! Built-in tools: bash, read, write, edit.

use crate::{Tool, ToolOutput, new_tool, new_tool_with_progress};
use serde::Deserialize;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;

const MAX_OUTPUT: usize = 16 * 1024;
const DEFAULT_BASH_TIMEOUT: u64 = 120;
const KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Strip control characters (except tab/newline/CR) and Unicode format
/// interlinear annotation marks from tool output before it reaches the model.
fn sanitize(s: &str) -> String {
    s.chars()
        .filter(|&c| {
            let code = c as u32;
            code == 0x09
                || code == 0x0a
                || code == 0x0d
                || !(code <= 0x1f || (0xfff9..=0xfffb).contains(&code))
        })
        .collect()
}

fn read_file_tail(path: &std::path::Path) -> String {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else {
        return String::new();
    };
    let Ok(len) = f.metadata().map(|m| m.len()) else {
        return String::new();
    };
    let start = len.saturating_sub(MAX_OUTPUT as u64);
    if f.seek(SeekFrom::Start(start)).is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if f.read_to_end(&mut buf).is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

#[derive(Deserialize)]
struct BashArgs {
    command: String,
    #[serde(default)]
    timeout: Option<u64>,
}

static BASH_TAG: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

const CHILD_SLOTS: usize = 32;
static CHILD_PGIDS: [std::sync::atomic::AtomicI32; CHILD_SLOTS] =
    [const { std::sync::atomic::AtomicI32::new(0) }; CHILD_SLOTS];

fn register_pgid(pgid: i32) -> bool {
    if pgid <= 0 {
        return false;
    }
    for slot in &CHILD_PGIDS {
        if slot
            .compare_exchange(
                0,
                pgid,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Relaxed,
            )
            .is_ok()
        {
            return true;
        }
    }
    false
}

fn unregister_pgid(pgid: i32) {
    for slot in &CHILD_PGIDS {
        let _ = slot.compare_exchange(
            pgid,
            0,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Relaxed,
        );
    }
}

/// Kill every live bash process group. Safe from a signal handler.
pub fn kill_children() {
    for slot in &CHILD_PGIDS {
        let pgid = slot.swap(0, std::sync::atomic::Ordering::AcqRel);
        if pgid > 0 {
            unsafe { libc::kill(-pgid, libc::SIGKILL) };
        }
    }
}

unsafe extern "C" fn sigint_reap_children(_: libc::c_int) {
    kill_children();
    unsafe {
        libc::signal(libc::SIGINT, libc::SIG_DFL);
        libc::raise(libc::SIGINT);
    }
}

fn ensure_sigint_handler() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        libc::signal(
            libc::SIGINT,
            sigint_reap_children as unsafe extern "C" fn(libc::c_int) as usize,
        );
    });
}

struct PgidGuard(i32);

impl Drop for PgidGuard {
    fn drop(&mut self) {
        unregister_pgid(self.0);
    }
}

#[cfg(target_os = "linux")]
fn child_pidfd(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) as i32 };
    if fd < 0 {
        None
    } else {
        Some(unsafe { OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(not(target_os = "linux"))]
fn child_pidfd(_: u32) -> Option<OwnedFd> {
    None
}

fn wait_for_child(pidfd: Option<&OwnedFd>, timeout: std::time::Duration) {
    let Some(pidfd) = pidfd else {
        std::thread::sleep(timeout.min(std::time::Duration::from_millis(1)));
        return;
    };
    let mut fd = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let millis = timeout.as_millis().min(i32::MAX as u128) as i32;
    unsafe {
        libc::poll(&mut fd, 1, millis.max(1));
    }
}

/// Reap a killed child, giving up after `grace` so an unkillable process
/// (uninterruptible I/O, frozen cgroup, stopped tracer) can never hang the
/// tool past its timeout.
fn reap(child: &mut std::process::Child, grace: std::time::Duration) {
    let until = std::time::Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) | Err(_) => return,
            Ok(None) => {
                if std::time::Instant::now() >= until {
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
        }
    }
}

pub fn bash(dir: &str) -> Tool {
    let dir = dir.to_string();
    let mut t = new_tool_with_progress(
        "bash",
        "Execute a bash command in the current working directory. Returns stdout and stderr. Output is truncated to last 16KB. The default timeout is 120 seconds.",
        r#"{"type":"object","properties":{"command":{"type":"string","description":"bash command to run"},"timeout":{"type":"integer","description":"Timeout in seconds (default: 120)"}},"required":["command"]}"#,
        move |a: BashArgs, progress: &mut dyn FnMut(&str)| {
            if a.timeout == Some(0) {
                return "error: invalid timeout: must be a positive number of seconds".to_string();
            }
            let timeout = a.timeout.unwrap_or(DEFAULT_BASH_TIMEOUT);
            let tag = BASH_TAG.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ensure_sigint_handler();
            let out_path =
                std::env::temp_dir().join(format!("axe-bash-{}-{tag}.out", std::process::id()));
            let out_file = match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&out_path)
            {
                Ok(file) => file,
                Err(e) => return format!("error: {e}"),
            };
            let err_file = match out_file.try_clone() {
                Ok(file) => file,
                Err(e) => {
                    let _ = std::fs::remove_file(&out_path);
                    return format!("error: {e}");
                }
            };
            let mut cmd = std::process::Command::new("bash");
            cmd.arg("-c").arg(&a.command);
            if !dir.is_empty() {
                cmd.current_dir(&dir);
            }
            cmd.stdout(std::process::Stdio::from(out_file));
            cmd.stderr(std::process::Stdio::from(err_file));
            cmd.process_group(0);
            let mut child = match cmd.spawn() {
                Ok(child) => child,
                Err(e) => {
                    let _ = std::fs::remove_file(&out_path);
                    return format!("error: {e}");
                }
            };
            // Registered so sigint_reap_children can kill the group if axe
            // dies first; dropped (unregistered) when the child is reaped.
            let pgid = child.id() as i32;
            if !register_pgid(pgid) {
                unsafe { libc::kill(-pgid, libc::SIGKILL) };
                reap(&mut child, KILL_GRACE);
                return "error: too many live bash processes".to_string();
            }
            let _guard = PgidGuard(pgid);
            let pidfd = child_pidfd(child.id());
            let mut exit: Option<std::process::ExitStatus> = None;
            let mut timed_out = false;
            let started = std::time::Instant::now();
            let timeout_dur = std::time::Duration::from_secs(timeout);
            let mut last_progress = std::time::Instant::now();
            loop {
                match child.try_wait() {
                    Ok(Some(st)) => {
                        exit = Some(st);
                        break;
                    }
                    Ok(None) => {
                        if started.elapsed() >= timeout_dur {
                            unsafe {
                                libc::kill(-(child.id() as i32), libc::SIGKILL);
                            }
                            reap(&mut child, KILL_GRACE);
                            timed_out = true;
                            break;
                        }
                        if last_progress.elapsed() >= std::time::Duration::from_millis(100) {
                            last_progress = std::time::Instant::now();
                            let tail = read_file_tail(&out_path);
                            if !tail.is_empty() {
                                progress(&sanitize(&tail));
                            }
                        }
                        let progress_wait = std::time::Duration::from_millis(100)
                            .saturating_sub(last_progress.elapsed());
                        let timeout_wait = timeout_dur.saturating_sub(started.elapsed());
                        wait_for_child(pidfd.as_ref(), progress_wait.min(timeout_wait));
                    }
                    Err(e) => {
                        unsafe {
                            libc::kill(-(child.id() as i32), libc::SIGKILL);
                        }
                        reap(&mut child, KILL_GRACE);
                        let _ = std::fs::remove_file(&out_path);
                        return format!("error: {e}");
                    }
                }
            }
            let truncated = std::fs::metadata(&out_path)
                .map(|metadata| metadata.len() > MAX_OUTPUT as u64)
                .unwrap_or(false);
            let output = if truncated {
                read_file_tail(&out_path)
            } else {
                std::fs::read(&out_path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default()
            };
            let mut display = sanitize(&output);
            if truncated {
                display.push_str(&format!(
                    "\n\n[Output truncated to the last 16KB. Full output: {}]",
                    out_path.display()
                ));
            } else {
                let _ = std::fs::remove_file(&out_path);
            }
            if timed_out {
                if !display.is_empty() && !display.ends_with('\n') {
                    display.push('\n');
                }
                display.push_str(&format!("error: command timed out after {timeout} seconds"));
            } else if let Some(st) = exit
                && !st.success()
            {
                if !display.is_empty() && !display.ends_with('\n') {
                    display.push('\n');
                }
                display.push_str(&format!("error: {}", status_str(st)));
            }
            display
        },
    );
    t.snippet = "Execute bash commands (ls, grep, find, etc.)";
    t
}

fn status_str(st: std::process::ExitStatus) -> String {
    match st.code() {
        Some(code) => format!("exit status {code}"),
        None => format!("signal: {st:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{apply_edits, sanitize};

    #[test]
    fn sanitize_strips_control_characters() {
        assert_eq!(sanitize("a\x00b\x1bc\x7fd"), "abc\u{7f}d");
        assert_eq!(sanitize("\x1b[31mred\x1b[0m"), "[31mred[0m");
        assert_eq!(sanitize("keep\tnewline\ncr\r"), "keep\tnewline\ncr\r");
        assert_eq!(sanitize("\u{fff9}fmt\u{fffb}"), "fmt");
        assert_eq!(sanitize("emoji 🙈 ok"), "emoji 🙈 ok");
    }

    #[test]
    fn edit_preserves_mixed_line_endings() {
        // CRLF file with one LF-only line: editing the CRLF part must not
        // rewrite the LF line's ending.
        let body = "a\r\nb\nc\r\n";
        let edits = vec![super::EditArg {
            old_text: "a\r\n".into(),
            new_text: "A\r\n".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap();
        assert_eq!(out, "A\r\nb\nc\r\n");

        // Editing the LF-only line leaves CRLF lines alone.
        let edits = vec![super::EditArg {
            old_text: "b".into(),
            new_text: "B\nB2".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap();
        assert_eq!(out, "a\r\nB\nB2\nc\r\n");
    }

    #[test]
    fn edit_crlf_roundtrip() {
        let body = "one\r\ntwo\r\nthree\r\n";
        let edits = vec![super::EditArg {
            old_text: "two".into(),
            new_text: "TWO\nTWO2".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap();
        assert_eq!(out, "one\r\nTWO\r\nTWO2\r\nthree\r\n");
    }

    #[test]
    fn edit_multibyte_content() {
        let body = "héllo wörld 🙈\nsecond\n";
        let edits = vec![super::EditArg {
            old_text: "wörld".into(),
            new_text: "planet".into(),
        }];
        let out = apply_edits("f", body, &edits).unwrap();
        assert_eq!(out, "héllo planet 🙈\nsecond\n");
    }

    #[test]
    fn edit_rejects_invalid_replacements() {
        let edit = |old: &str, new: &str| super::EditArg {
            old_text: old.into(),
            new_text: new.into(),
        };
        let error = apply_edits("f", "x x", &[edit("x", "y")]).unwrap_err();
        assert!(error.contains("2 occurrences"), "{error}");
        let error = apply_edits("f", "abc", &[edit("abc", "x"), edit("bc", "y")]).unwrap_err();
        assert!(error.contains("overlap"), "{error}");
        let error = apply_edits("f", "x", &[edit("", "y")]).unwrap_err();
        assert!(error.contains("must not be empty"), "{error}");
        let error = apply_edits("f", "x", &[edit("x", "x")]).unwrap_err();
        assert!(error.contains("No changes made"), "{error}");
    }

    #[test]
    fn edit_preserves_bom() {
        let edits = [super::EditArg {
            old_text: "old".into(),
            new_text: "new".into(),
        }];
        assert_eq!(
            apply_edits("f", "\u{feff}old", &edits).unwrap(),
            "\u{feff}new"
        );
    }

    #[test]
    fn read_accepts_float_offset() {
        let dir = std::env::temp_dir().join(format!("axe-read-float-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("f.txt");
        std::fs::write(&path, "a\nb\nc\n").unwrap();
        let read = crate::tools::read();
        let args = format!(
            r#"{{"path":"{}","offset":2.0,"limit":1.0}}"#,
            path.display()
        );
        let out = (read.run)(&args, &mut |_| {}).text;
        assert!(out.starts_with("b"), "got: {out}");
        assert!(!out.contains("invalid arguments"), "got: {out}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn read_attaches_images_and_rejects_binary() {
        let dir = std::env::temp_dir().join(format!("axe-read-image-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let read = crate::tools::read();

        // A real PNG signature followed by NUL bytes: the image check must win
        // over the binary guard so the model can see the picture.
        let png = dir.join("shot.png");
        std::fs::write(
            &png,
            [0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 0, 0, 0],
        )
        .unwrap();
        let out = (read.run)(&format!(r#"{{"path":"{}"}}"#, png.display()), &mut |_| {});
        assert_eq!(out.images.len(), 1, "text: {}", out.text);
        assert!(out.images[0].url.starts_with("data:image/png;base64,"));
        assert!(out.text.contains("shot.png"), "got: {}", out.text);

        // Other binaries get a clear error rather than 16KB of mojibake.
        let bin = dir.join("thing.bin");
        std::fs::write(&bin, [0u8, 1, 2, 3, 0, 4]).unwrap();
        let out = (read.run)(&format!(r#"{{"path":"{}"}}"#, bin.display()), &mut |_| {});
        assert!(out.images.is_empty());
        assert!(out.text.contains("binary file"), "got: {}", out.text);

        let txt = dir.join("t.txt");
        std::fs::write(&txt, "hello\n").unwrap();
        let out = (read.run)(&format!(r#"{{"path":"{}"}}"#, txt.display()), &mut |_| {});
        assert!(out.images.is_empty());
        assert_eq!(out.text, "hello");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn kill_children_reaps_spawned_group() {
        use std::os::unix::process::CommandExt;
        let _lock = BASH_TEST.lock().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let pgid = child.id() as i32;
        assert!(super::register_pgid(pgid));
        crate::tools::kill_children();
        let st = child.wait().unwrap();
        assert!(!st.success());
    }

    static BASH_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn reap_gives_up_on_live_child() {
        use std::os::unix::process::CommandExt;
        let _lock = BASH_TEST.lock().unwrap();
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .process_group(0)
            .spawn()
            .unwrap();
        let start = std::time::Instant::now();
        super::reap(&mut child, std::time::Duration::from_millis(200));
        assert!(start.elapsed() < std::time::Duration::from_secs(2));
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.wait();
    }

    #[test]
    fn bash_truncation_notice() {
        let _lock = BASH_TEST.lock().unwrap();
        let bash = crate::tools::bash("");
        let args = serde_json::json!({"command": "yes | head -c 20000"}).to_string();
        let out = (bash.run)(&args, &mut |_| {}).text;
        assert!(
            out.contains("Output truncated to the last 16KB"),
            "got tail: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(
            out.contains("Full output:"),
            "got: {}",
            &out[out.len().saturating_sub(200)..]
        );
        assert!(
            !out.contains("[truncated]"),
            "got: {}",
            &out[out.len().saturating_sub(200)..]
        );
        let path = out
            .split("Full output: ")
            .nth(1)
            .and_then(|s| s.trim().split(']').next())
            .unwrap();
        assert!(
            std::path::Path::new(path).exists(),
            "full output file missing: {path}"
        );
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

/// Read a file as text lines, with offset/limit paging and truncation.
fn read_text(a: &ReadArgs) -> String {
    use std::io::BufRead;
    let file = match std::fs::File::open(&a.path) {
        Ok(file) => file,
        Err(e) => return format!("error: {e}"),
    };
    let start = a.offset.unwrap_or(1).saturating_sub(1);
    let limit = a.limit.unwrap_or(usize::MAX);
    let mut reader = std::io::BufReader::new(file);
    let mut line = Vec::new();
    let mut output = String::new();
    let mut total = 0usize;
    let mut skipped_partial = false;
    while total < start {
        let buf = match reader.fill_buf() {
            Ok(buf) => buf,
            Err(e) => return format!("error: {e}"),
        };
        if buf.is_empty() {
            total += usize::from(skipped_partial);
            break;
        }
        let mut consumed = buf.len();
        for (index, byte) in buf.iter().enumerate() {
            if *byte == b'\n' {
                total += 1;
                skipped_partial = false;
                if total == start {
                    consumed = index + 1;
                    break;
                }
            } else {
                skipped_partial = true;
            }
        }
        reader.consume(consumed);
    }
    let mut shown = 0usize;
    let mut overflow = false;
    let mut oversized = None;
    loop {
        if shown >= limit || overflow {
            let mut trailing = false;
            loop {
                let buf = match reader.fill_buf() {
                    Ok(buf) => buf,
                    Err(e) => return format!("error: {e}"),
                };
                if buf.is_empty() {
                    break;
                }
                for &byte in buf {
                    if byte == b'\n' {
                        total += 1;
                        trailing = false;
                    } else {
                        trailing = true;
                    }
                }
                let len = buf.len();
                reader.consume(len);
            }
            total += usize::from(trailing);
            break;
        }
        line.clear();
        let read = match reader.read_until(b'\n', &mut line) {
            Ok(read) => read,
            Err(e) => return format!("error: {e}"),
        };
        if read == 0 {
            break;
        }
        if line.last() == Some(&b'\n') {
            line.pop();
        }
        let index = total;
        total += 1;
        if index < start || index >= start.saturating_add(limit) || overflow {
            continue;
        }
        let text = sanitize(&String::from_utf8_lossy(&line));
        if shown == 0 && text.len() > MAX_OUTPUT {
            oversized = Some((index + 1, text.len()));
            overflow = true;
            continue;
        }
        let separator = usize::from(!output.is_empty());
        if output.len() + separator + text.len() > MAX_OUTPUT {
            overflow = true;
            continue;
        }
        if separator == 1 {
            output.push('\n');
        }
        output.push_str(&text);
        shown += 1;
    }
    if a.offset.is_some() && start >= total {
        return format!(
            "error: offset {} is beyond end of file ({} lines total)",
            a.offset.unwrap_or(1),
            total
        );
    }
    if let Some((line, bytes)) = oversized {
        return format!("[Line {line} is {bytes} bytes, exceeds the {MAX_OUTPUT} limit.]");
    }
    let end = start.saturating_add(shown).min(total);
    if overflow {
        return format!(
            "{}\n\n[Showing lines {}-{} of {} ({} limit). Use offset={} to continue.]",
            output,
            start + 1,
            end,
            total,
            MAX_OUTPUT,
            end + 1
        );
    }
    let remaining = total.saturating_sub(start.saturating_add(limit).min(total));
    if remaining > 0 {
        output.push_str(&format!(
            "\n\n[{remaining} more lines in file. Use offset={} to continue.]",
            start.saturating_add(limit) + 1
        ));
    }
    output
}

/// A NUL byte in the first block means this is not a text file. Images are
/// handled before this; anything else would only poison the context.
fn looks_binary(path: &str) -> bool {
    use std::io::Read;
    let Ok(mut file) = std::fs::File::open(path) else {
        return false;
    };
    let mut head = [0u8; 8192];
    let Ok(read) = file.read(&mut head) else {
        return false;
    };
    head[..read].contains(&0)
}

pub fn read() -> Tool {
    let mut t = new_tool(
        "read",
        "Read the contents of a file. Output is truncated to 16KB. Use offset/limit for large files. When you need the full file, continue with the suggested offset. Reading a JPEG, PNG, GIF, or WebP attaches the image so you can see it.",
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to read (relative or absolute)"},"offset":{"type":"integer","description":"Line number to start reading from (1-indexed)"},"limit":{"type":"integer","description":"Maximum number of lines to read"}},"required":["path"]}"#,
        |a: ReadArgs| {
            if let Some(image) = crate::image::attach_if_image(&a.path) {
                return ToolOutput {
                    text: format!("Attached {} for viewing.", a.path),
                    images: vec![image],
                };
            }
            if looks_binary(&a.path) {
                return ToolOutput::text(format!(
                    "error: {} is a binary file; read handles text files and JPEG, PNG, GIF, and WebP images",
                    a.path
                ));
            }
            ToolOutput::text(read_text(&a))
        },
    );
    t.snippet = "Read file contents (truncated, use offset to continue); images are attached so you can see them";
    t
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

pub fn write() -> Tool {
    let mut t = new_tool(
        "write",
        "Write content to a file. Creates the file if it doesn't exist, overwrites if it does. Automatically creates parent directories.",
        r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to write (relative or absolute)"},"content":{"type":"string","description":"Content to write to the file"}},"required":["path","content"]}"#,
        |a: WriteArgs| match crate::atomic_write(
            std::path::Path::new(&a.path),
            a.content.as_bytes(),
        ) {
            Ok(()) => format!("wrote {} ({} bytes)", a.path, a.content.len()),
            Err(e) => format!("error: {e}"),
        },
    );
    t.sequential = true;
    t.snippet = "Write content to a file";
    t
}

#[derive(Deserialize)]
struct EditArg {
    #[serde(rename = "oldText")]
    old_text: String,
    #[serde(rename = "newText")]
    new_text: String,
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    edits: Vec<EditArg>,
}

fn normalize_lf(s: &str) -> std::borrow::Cow<'_, str> {
    if s.contains('\r') {
        std::borrow::Cow::Owned(s.replace("\r\n", "\n").replace('\r', "\n"))
    } else {
        std::borrow::Cow::Borrowed(s)
    }
}

/// Byte-offset map from `normalize_lf(body)` back into `body`: entry i is
/// the body offset of normalized offset i. CRLF collapses to one byte, a
/// lone CR becomes one LF byte, everything else maps 1:1.
fn lf_map(body: &str) -> Vec<usize> {
    let b = body.as_bytes();
    let mut map = Vec::with_capacity(b.len() + 1);
    let mut i = 0;
    while i < b.len() {
        map.push(i);
        if b[i] == b'\r' {
            i += if i + 1 < b.len() && b[i + 1] == b'\n' {
                2
            } else {
                1
            };
        } else {
            i += 1;
        }
    }
    map.push(b.len());
    map
}

/// Re-apply `ending` to an LF-normalized string.
fn with_ending(s: &str, ending: &str) -> String {
    if ending == "\r\n" {
        s.split('\n').collect::<Vec<_>>().join("\r\n")
    } else {
        s.to_string()
    }
}

/// Line ending to use for newlines introduced into `body[bs..be]`: prefer
/// the region's own, then the terminator right after / before it, then the
/// file's dominant style.
fn region_ending(body: &str, bs: usize, be: usize) -> &'static str {
    if body[bs..be].contains("\r\n") || body[be..].starts_with("\r\n") {
        return "\r\n";
    }
    if body[be..].starts_with('\n') {
        return "\n";
    }
    if body[..bs].ends_with("\r\n") {
        return "\r\n";
    }
    if body[..bs].ends_with('\n') {
        return "\n";
    }
    if body.contains("\r\n") { "\r\n" } else { "\n" }
}

fn empty_old_error(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!("error: oldText must not be empty in {path}.")
    } else {
        format!("error: edits[{i}].oldText must not be empty in {path}.")
    }
}

fn not_found_error(path: &str, i: usize, total: usize) -> String {
    if total == 1 {
        format!(
            "error: Could not find the exact text in {path}. The old text must match exactly including all whitespace and newlines."
        )
    } else {
        format!(
            "error: Could not find edits[{i}] in {path}. The oldText must match exactly including all whitespace and newlines."
        )
    }
}

fn duplicate_error(path: &str, i: usize, total: usize, n: usize) -> String {
    if total == 1 {
        format!(
            "error: Found {n} occurrences of the text in {path}. The text must be unique. Please provide more context to make it unique."
        )
    } else {
        format!(
            "error: Found {n} occurrences of edits[{i}] in {path}. Each oldText must be unique. Please provide more context to make it unique."
        )
    }
}

fn no_change_error(path: &str, total: usize) -> String {
    if total == 1 {
        format!("error: No changes made to {path}. The replacement produced identical content.")
    } else {
        format!("error: No changes made to {path}. The replacements produced identical content.")
    }
}

fn apply_edits(path: &str, content: &str, edits: &[EditArg]) -> Result<String, String> {
    if edits.is_empty() {
        return Err("error: edits must contain at least one replacement.".to_string());
    }
    let (bom, body) = match content.strip_prefix('\u{FEFF}') {
        Some(rest) => ("\u{FEFF}", rest),
        None => ("", content),
    };
    let normalized = normalize_lf(body);
    let mut olds = Vec::with_capacity(edits.len());
    for (i, e) in edits.iter().enumerate() {
        if e.old_text.is_empty() {
            return Err(empty_old_error(path, i, edits.len()));
        }
        olds.push(normalize_lf(&e.old_text));
    }
    let mut found: Vec<(usize, usize, usize)> = Vec::new();
    for (i, old) in olds.iter().enumerate() {
        let old = old.as_ref();
        let (start, len) = match normalized.find(old) {
            Some(idx) => {
                let remaining = &normalized[idx + old.len()..];
                if remaining.contains(old) {
                    let n = 1 + remaining.matches(old).count();
                    return Err(duplicate_error(path, i, edits.len(), n));
                }
                (idx, old.len())
            }
            None => return Err(not_found_error(path, i, edits.len())),
        };
        found.push((i, start, len));
    }
    found.sort_by_key(|f| f.1);
    for w in found.windows(2) {
        if w[0].1 + w[0].2 > w[1].1 {
            return Err(format!(
                "error: edits[{}] and edits[{}] overlap in {path}. Merge them into one edit or target disjoint regions.",
                w[0].0, w[1].0
            ));
        }
    }
    // Apply in body coordinates so each region keeps its own line endings:
    // untouched lines are never rewritten, even in files with mixed endings.
    let map = body.contains('\r').then(|| lf_map(body));
    let mut out = body.to_string();
    for &(i, start, len) in found.iter().rev() {
        let bs = map.as_ref().map_or(start, |map| map[start]);
        let be = map.as_ref().map_or(start + len, |map| map[start + len]);
        let replacement = with_ending(
            &normalize_lf(&edits[i].new_text),
            region_ending(body, bs, be),
        );
        out.replace_range(bs..be, &replacement);
    }
    if out == body {
        return Err(no_change_error(path, edits.len()));
    }
    Ok(format!("{bom}{out}"))
}

const EDIT_SCHEMA: &str = r#"{"type":"object","properties":{"path":{"type":"string","description":"Path to the file to edit (relative or absolute)"},"edits":{"type":"array","description":"One or more targeted replacements. Each edit is matched against the original file, not incrementally. Do not include overlapping or nested edits. If two changes touch the same block or nearby lines, merge them into one edit instead.","items":{"type":"object","properties":{"oldText":{"type":"string","description":"Exact text for one targeted replacement. It must be unique in the original file and must not overlap with any other edits[].oldText in the same call."},"newText":{"type":"string","description":"Replacement text for this targeted edit."}},"required":["oldText","newText"]}}},"required":["path","edits"]}"#;

pub fn edit() -> Tool {
    let mut t = new_tool(
        "edit",
        "Edit a single file using exact text replacement. Every edits[].oldText must match a unique, non-overlapping region of the original file. If two changes affect the same block or nearby lines, merge them into one edit instead of emitting overlapping edits. Do not include large unchanged regions just to connect distant changes.",
        EDIT_SCHEMA,
        |a: EditArgs| {
            if a.edits.is_empty() {
                return "error: edits must contain at least one replacement".into();
            }
            match std::fs::read_to_string(&a.path) {
                Err(e) => format!("error: {e}"),
                Ok(s) => match apply_edits(&a.path, &s, &a.edits) {
                    Ok(out) => {
                        let n = a.edits.len();
                        match crate::atomic_write(std::path::Path::new(&a.path), out.as_bytes()) {
                            Ok(()) => format!("Successfully replaced {n} block(s) in {}.", a.path),
                            Err(e) => format!("error: {e}"),
                        }
                    }
                    Err(e) => e,
                },
            }
        },
    );
    t.sequential = true;
    t.snippet = "Make precise file edits with exact text replacement, including multiple disjoint edits in one call";
    t
}
