use std::io::{self, IsTerminal, Write};

pub(super) enum State {
    Idle,
    Working,
    Error,
}

impl State {
    fn word(self) -> &'static str {
        match self {
            State::Idle => "idle",
            State::Working => "working",
            State::Error => "error",
        }
    }
}

pub(super) fn report(state: State) {
    emit(&format!("state={}:app=axe", state.word()));
}

pub(super) fn clear() {
    emit("state=clear:app=axe");
}

fn emit(body: &str) {
    if !io::stdout().is_terminal() {
        return;
    }
    let mut stdout = io::stdout().lock();
    let _ = write!(stdout, "\x1b]7501;{body}\x1b\\");
    let _ = stdout.flush();
}
