//! Signing this machine in to an OpenMausBot account, which `--tunnel` needs
//! for its public address. `openmausbot account login --email` emails an
//! 8-digit code and reads it from stdin; the screen sends the email first and
//! the code later, and the login process waits in between on a thread this
//! plugin owns (IPC never waits for it).
use crate::installer;
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, Command, Stdio},
    sync::{
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
        Arc, Mutex,
    },
    thread,
    time::Duration,
};

/// The code expires upstream after a few minutes; the process does not
/// outlive it by much.
const CODE_WAIT: Duration = Duration::from_secs(15 * 60);
const ASKS_FOR_CODE: &str = "8-digit code";

#[derive(Clone, Debug, PartialEq, Eq)]
enum Phase {
    Idle,
    Sending,
    CodeSent,
    Verifying,
    SignedIn,
    Failed(String),
}

#[derive(Default)]
pub struct Login {
    state: Mutex<State>,
}

struct State {
    phase: Phase,
    email: Option<String>,
    code: Option<Sender<String>>,
    /// Bumps on every attempt, so a finished older attempt does not overwrite
    /// a newer one.
    attempt: u64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            phase: Phase::Idle,
            email: None,
            code: None,
            attempt: 0,
        }
    }
}

impl Login {
    pub fn public(&self) -> Value {
        let state = self.state.lock().unwrap();
        let (phase, error) = match &state.phase {
            Phase::Idle => ("idle", None),
            Phase::Sending => ("sending", None),
            Phase::CodeSent => ("code_sent", None),
            Phase::Verifying => ("verifying", None),
            Phase::SignedIn => ("signed_in", None),
            Phase::Failed(error) => ("failed", Some(error.clone())),
        };
        json!({"phase": phase, "email": state.email, "error": error})
    }

    /// Starts `account login --email`; an earlier attempt still waiting for
    /// its code is dropped (its process ends when its stdin closes).
    pub fn start(
        self: &Arc<Self>,
        email: &str,
        base: &Path,
        data: &Path,
        home: &Path,
    ) -> Result<(), String> {
        let email = email.trim();
        if email.is_empty() || !email.contains('@') || email.chars().any(char::is_whitespace) {
            return Err("Enter a valid email address".into());
        }
        let mut command = Command::new(installer::node_binary(base));
        command
            .arg(installer::omb_cli(base))
            .args(["account", "login", "--email", email, "--data-dir"])
            .arg(data)
            .current_dir(data);
        installer::node_environment(&mut command, base, home);
        command.env("OMB_DATA_DIR", data);
        let (sender, receiver) = mpsc::channel();
        let attempt = {
            let mut state = self.state.lock().unwrap();
            state.attempt += 1;
            state.phase = Phase::Sending;
            state.email = Some(email.to_string());
            state.code = Some(sender);
            state.attempt
        };
        let this = Arc::clone(self);
        thread::spawn(move || {
            // The child's PDEATHSIG follows this thread.
            let reporter = Arc::clone(&this);
            let outcome = run(command, receiver, move |phase| {
                reporter.advance(attempt, phase)
            });
            this.advance(
                attempt,
                match outcome {
                    Ok(()) => Phase::SignedIn,
                    Err(error) => Phase::Failed(error),
                },
            );
        });
        Ok(())
    }

    pub fn code(&self, code: &str) -> Result<(), String> {
        let code: String = code.chars().filter(|c| !c.is_whitespace() && *c != '-').collect();
        if code.len() != 8 || !code.chars().all(|c| c.is_ascii_digit()) {
            return Err("The code from the email has 8 digits".into());
        }
        let mut state = self.state.lock().unwrap();
        if state.phase != Phase::CodeSent {
            return Err("Ask for a new code first".into());
        }
        let sender = state.code.take().ok_or("Ask for a new code first")?;
        sender
            .send(code)
            .map_err(|_| "The sign-in stopped; ask for a new code")?;
        state.phase = Phase::Verifying;
        Ok(())
    }

    fn advance(&self, attempt: u64, phase: Phase) {
        let mut state = self.state.lock().unwrap();
        if state.attempt != attempt {
            return;
        }
        // The code was already sent: a late "code asked" line changes nothing.
        if phase == Phase::CodeSent && state.phase == Phase::Verifying {
            return;
        }
        if matches!(phase, Phase::SignedIn | Phase::Failed(_)) {
            state.code = None;
        }
        state.phase = phase;
    }
}

fn run(
    mut command: Command,
    code: Receiver<String>,
    report: impl Fn(Phase) + Send + Sync + 'static,
) -> Result<(), String> {
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    own_death(&mut command);
    let mut child = command
        .spawn()
        .map_err(|_| "Cannot start the OpenMausBot sign-in")?;
    let mut stdin = child.stdin.take();
    let report = Arc::new(report);
    let last = Arc::new(Mutex::new(String::new()));
    let readers = [
        child.stdout.take().map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
        child.stderr.take().map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
    ]
    .into_iter()
    .flatten()
    .map(|stream| {
        let (report, last) = (Arc::clone(&report), Arc::clone(&last));
        thread::spawn(move || read_lines(stream, report.as_ref(), &last))
    })
    .collect::<Vec<_>>();

    let result = match code.recv_timeout(CODE_WAIT) {
        Ok(code) => {
            send_code(&mut stdin, &code);
            drop(stdin);
            finish(&mut child)
        }
        Err(RecvTimeoutError::Timeout) => {
            let _ = child.kill();
            let _ = child.wait();
            Err("The code expired; ask for a new one".to_string())
        }
        Err(RecvTimeoutError::Disconnected) => {
            // Superseded or the plugin is closing: end quietly.
            drop(stdin);
            let _ = child.kill();
            finish(&mut child)
        }
    };
    for reader in readers {
        let _ = reader.join();
    }
    result.map_err(|error| {
        let detail = last.lock().unwrap().clone();
        if detail.is_empty() {
            error
        } else {
            detail
        }
    })
}

fn read_lines(
    stream: Box<dyn std::io::Read + Send>,
    report: &(dyn Fn(Phase) + Send + Sync),
    last: &Mutex<String>,
) {
    // The code question has no newline: read by byte chunks, not by lines.
    let mut reader = BufReader::new(stream);
    let mut pending = String::new();
    loop {
        let chunk = match reader.fill_buf() {
            Ok([]) | Err(_) => break,
            Ok(bytes) => String::from_utf8_lossy(bytes).into_owned(),
        };
        reader.consume(chunk.len());
        pending.push_str(&chunk);
        if pending.contains(ASKS_FOR_CODE) {
            report(Phase::CodeSent);
            pending.clear();
            continue;
        }
        while let Some(end) = pending.find('\n') {
            let line = strip_ansi(pending[..end].trim());
            if !line.is_empty() {
                *last.lock().unwrap() = line;
            }
            pending.drain(..=end);
        }
    }
}

fn send_code(stdin: &mut Option<ChildStdin>, code: &str) {
    if let Some(stdin) = stdin.as_mut() {
        let _ = stdin.write_all(format!("{code}\n").as_bytes());
        let _ = stdin.flush();
    }
}

fn finish(child: &mut Child) -> Result<(), String> {
    match child.wait() {
        Ok(exit) if exit.success() => Ok(()),
        _ => Err("Sign-in failed".into()),
    }
}

fn own_death(command: &mut Command) {
    #[cfg(target_os = "linux")]
    unsafe {
        use std::os::unix::process::CommandExt;
        command.pre_exec(|| {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

fn strip_ansi(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' {
            if chars.peek() == Some(&'[') {
                chars.next();
                for next in chars.by_ref() {
                    if next.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            continue;
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_the_email_and_the_code_shape() {
        let login = Arc::new(Login::default());
        let tmp = std::env::temp_dir();
        assert!(login.start("not-an-email", &tmp, &tmp, &tmp).is_err());
        assert!(login.code("1234").is_err());
        assert_eq!(login.code("1234-5678"), Err("Ask for a new code first".into()));
    }

    #[test]
    fn walks_a_fake_login_to_signed_in() {
        let login = Arc::new(Login::default());
        let (sender, receiver) = mpsc::channel();
        {
            let mut state = login.state.lock().unwrap();
            state.attempt = 1;
            state.phase = Phase::Sending;
            state.code = Some(sender);
        }
        let script = "printf 'Enter the 8-digit code we emailed to a@b: '; read code; \
                      [ \"$code\" = 12345678 ] && echo ok || { echo 'sign-in failed: bad code'; exit 1; }";
        let mut command = Command::new("sh");
        command.args(["-c", script]);
        let this = Arc::clone(&login);
        let worker = thread::spawn(move || {
            let outcome = run(command, receiver, {
                let this = Arc::clone(&this);
                move |phase| this.advance(1, phase)
            });
            this.advance(
                1,
                match outcome {
                    Ok(()) => Phase::SignedIn,
                    Err(error) => Phase::Failed(error),
                },
            );
        });
        for _ in 0..50 {
            if login.public()["phase"] == "code_sent" {
                break;
            }
            thread::sleep(Duration::from_millis(50));
        }
        login.code("1234 5678").unwrap();
        worker.join().unwrap();
        assert_eq!(login.public()["phase"], "signed_in");
    }

    #[test]
    fn strips_terminal_colours() {
        assert_eq!(strip_ansi("\u{1b}[31msign-in failed\u{1b}[0m"), "sign-in failed");
    }
}
