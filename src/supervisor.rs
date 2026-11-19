//! Background install and the supervised OpenMausBot server. IPC never waits
//! for a download or a start: `query` only reads the status snapshot.
use crate::{
    access::{self, Access, Way},
    account::Login,
    bridge::{self, Bridge},
    context::Context,
    installer::{self, InstallMode, Step},
    plugin_directory,
};
use serde_json::{json, Value};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};

const START_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Clone)]
pub struct Status {
    pub phase: String,
    pub detail: String,
    port: Option<u16>,
    token: Option<String>,
    step: Option<(Step, Instant, Option<f64>)>,
    mode: Option<&'static str>,
}

impl Status {
    fn new(phase: &str, detail: &str) -> Self {
        Self {
            phase: phase.into(),
            detail: detail.into(),
            port: None,
            token: None,
            step: None,
            mode: None,
        }
    }

    pub fn public(&self) -> Value {
        let mut public = json!({"status": self.phase, "detail": self.detail});
        if self.phase == "installing" {
            if let Some((step, since, measured)) = self.step {
                public["step"] = json!(step.code());
                public["progress"] = json!(progress_at(step, since.elapsed(), measured));
            }
            if let Some(mode) = self.mode {
                public["mode"] = json!(mode);
            }
        }
        public
    }

    pub fn upstream(&self) -> Value {
        match self.phase.as_str() {
            "ready" => json!({"status": "ready", "port": self.port, "token": self.token}),
            "error" => json!({"status": "error", "error": self.detail}),
            _ => json!({"status": "starting"}),
        }
    }
}

/// Each step's share of the bar, `(start, end)`, and for an unmeasured step
/// the time constant of its creep.
fn weights(step: Step) -> (f64, f64, f64) {
    match step {
        Step::Node => (0.0, 40.0, 0.0),
        Step::Packages => (40.0, 95.0, 40.0),
        Step::Connect => (95.0, 100.0, 5.0),
    }
}

/// A download follows its bytes, any other step creeps toward its end with
/// time; neither reaches the end before the step really finished.
pub fn progress_at(step: Step, elapsed: Duration, measured: Option<f64>) -> u8 {
    let (start, end, tau) = weights(step);
    let fraction = match measured {
        Some(done) => done.clamp(0.0, 1.0),
        None if tau > 0.0 => 1.0 - (-elapsed.as_secs_f64() / tau).exp(),
        None => 0.0,
    };
    (start + (end - start) * 0.95 * fraction).floor() as u8
}

struct Running {
    child: Child,
    _bridge: Option<Bridge>,
}

pub struct Supervisor {
    status: Mutex<Status>,
    process: Mutex<Option<Running>>,
    installer: Mutex<Option<Child>>,
    context: Mutex<Context>,
    busy: Mutex<bool>,
    login: Arc<Login>,
}

impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            status: Mutex::new(Status::new("waiting", "Install OpenMausBot to begin")),
            process: Mutex::new(None),
            installer: Mutex::new(None),
            context: Mutex::new(Context::default()),
            busy: Mutex::new(false),
            login: Arc::new(Login::default()),
        })
    }

    pub fn status(&self) -> Status {
        self.status.lock().unwrap().clone()
    }

    fn set(&self, phase: &str, detail: &str) {
        *self.status.lock().unwrap() = Status::new(phase, detail);
    }

    /// Keeps the node's context; an installed OpenMausBot starts at once (the
    /// plugin is on demand: the host only runs while the screen is open). A
    /// first install waits for the screen, which is the opt-in.
    pub fn configure(self: &Arc<Self>, context: Context) {
        *self.context.lock().unwrap() = context;
        if plugin_directory().is_ok_and(|base| base.join(".installed").exists()) {
            self.launch();
        }
    }

    /// `None` while another launch runs; `Some(false)` when the server is up;
    /// `Some(true)` when the caller starts it, with the phase already off any
    /// previous error so the screen's next poll sees the new attempt.
    fn begin_launch(&self) -> Option<bool> {
        let mut busy = self.busy.lock().unwrap();
        if *busy {
            return None;
        }
        if self.status().phase == "ready" {
            return Some(false);
        }
        *busy = true;
        self.set("starting", "Starting OpenMausBot");
        Some(true)
    }

    pub fn launch(self: &Arc<Self>) -> bool {
        match self.begin_launch() {
            None => return false,
            Some(false) => return true,
            Some(true) => {}
        }
        let this = Arc::clone(self);
        thread::spawn(move || {
            // This thread lives as long as the server: the child's
            // PDEATHSIG follows the thread that spawned it.
            let result = this.start();
            if let Err(error) = result {
                this.stop();
                this.set("error", &error);
            }
            *this.busy.lock().unwrap() = false;
        });
        true
    }

    /// The phone access settings, what each way needs and the sign-in state.
    pub fn access(&self) -> Result<Value, String> {
        let base = plugin_directory()?;
        let data = self.data_directory(&base);
        let mut reply = Access::load(&base).to_json();
        reply["tunnel_account"] = json!(access::tunnel_account_saved(&data));
        reply["tailscale_installed"] = json!(access::tailscale_installed());
        reply["login"] = self.login.public();
        Ok(reply)
    }

    /// Saves the settings and restarts the server with them.
    pub fn set_access(self: &Arc<Self>, request: &Value) -> Result<Value, String> {
        let base = plugin_directory()?;
        let data = self.data_directory(&base);
        let access = Access::from_json(request)?;
        access.validate()?;
        if access.way == Way::Tunnel && !access::tunnel_account_saved(&data) {
            return Err("Sign in to an OpenMausBot account before turning on the tunnel".into());
        }
        if access.way == Way::Tailscale && !access::tailscale_installed() {
            return Err("Tailscale is not installed on this machine".into());
        }
        fs::create_dir_all(&base).map_err(|_| "Cannot create OpenMausBot runtime directory")?;
        access.save(&base)?;
        self.restart();
        self.access()
    }

    pub fn login_start(&self, email: &str) -> Result<Value, String> {
        let base = plugin_directory()?;
        if !matches!(installer::install_mode(&base), InstallMode::Installed) {
            return Err("Install OpenMausBot first".into());
        }
        let data = self.data_directory(&base);
        let home = data.join("home");
        fs::create_dir_all(&home).map_err(|_| "Cannot create OpenMausBot data directory")?;
        self.login.start(email, &base, &data, &home)?;
        Ok(self.login.public())
    }

    pub fn login_code(&self, code: &str) -> Result<Value, String> {
        self.login.code(code)?;
        Ok(self.login.public())
    }

    /// Stops the server and starts it again once the old launch thread ended.
    pub fn restart(self: &Arc<Self>) {
        let this = Arc::clone(self);
        thread::spawn(move || {
            this.stop();
            this.set("starting", "Restarting OpenMausBot");
            for _ in 0..100 {
                if !*this.busy.lock().unwrap() {
                    break;
                }
                thread::sleep(Duration::from_millis(100));
            }
            this.set("waiting", "Restarting OpenMausBot");
            this.launch();
        });
    }

    fn data_directory(&self, base: &Path) -> PathBuf {
        self.context
            .lock()
            .unwrap()
            .workspace_state_dir()
            .unwrap_or_else(|| base.join("data"))
    }

    fn start(self: &Arc<Self>) -> Result<(), String> {
        let base = plugin_directory()?;
        fs::create_dir_all(&base).map_err(|_| "Cannot create OpenMausBot runtime directory")?;
        private_directory(&base)?;
        self.stop();
        let data = self.data_directory(&base);
        let home = data.join("home");
        fs::create_dir_all(&home).map_err(|_| "Cannot create OpenMausBot data directory")?;
        private_directory(&data)?;

        let mode = match installer::install_mode(&base) {
            InstallMode::Fresh => Some("install"),
            InstallMode::Update => Some("update"),
            InstallMode::Installed => None,
        };
        if mode.is_some() {
            self.set("installing", "Installing OpenMausBot (see install.log)");
        }
        let report = |step: Step, measured: Option<f64>| {
            let Some(mode) = mode else { return };
            let mut status = self.status.lock().unwrap();
            let since = match status.step {
                Some((current, since, _)) if current == step => since,
                _ => Instant::now(),
            };
            status.step = Some((step, since, measured));
            status.mode = Some(mode);
        };
        installer::ensure(
            &base,
            &home,
            |command| self.run_install_step(command, &base),
            &report,
        )?;

        let gateway = self.context.lock().unwrap().gateway.clone();
        if let Some(gateway) = &gateway {
            report(Step::Connect, None);
            let model = installer::first_gateway_model(&gateway.openai_base_url, &gateway.api_key);
            installer::register_pom(&data, &gateway.openai_base_url, model.as_deref())?;
        }

        self.set("starting", "Starting OpenMausBot");
        let access = Access::load(&base);
        let port = if access.exposed() {
            fixed_port(access.port, access.lan)?
        } else {
            free_port()?
        };
        let webhook = free_port()?;
        let log =
            fs::File::create(base.join("server.log")).map_err(|_| "Cannot open server log")?;
        let errors = log.try_clone().map_err(|_| "Cannot open server log")?;
        let mut command = Command::new(installer::node_binary(&base));
        command
            .arg(installer::omb_cli(&base))
            .args([
                "serve",
                "--no-pair",
                "--port",
                &port.to_string(),
                "--data-dir",
            ])
            .arg(&data)
            .args(access.serve_args())
            .current_dir(&data);
        installer::node_environment(&mut command, &base, &home);
        command
            .env("OMB_DATA_DIR", &data)
            .env("OMB_WEBHOOK_PORT", webhook.to_string())
            .env("OMB_SERVER_NAME", "POM");
        if let Some(gateway) = &gateway {
            command.env(installer::POM_KEY_ENV, &gateway.api_key);
        }
        command.stdin(Stdio::null()).stdout(log).stderr(errors);
        own_process_group(&mut command);
        let child = command
            .spawn()
            .map_err(|_| "Cannot start OpenMausBot; see server.log")?;
        *self.process.lock().unwrap() = Some(Running {
            child,
            _bridge: None,
        });

        let deadline = Instant::now() + START_TIMEOUT;
        while !healthy(port) {
            if self.exited() {
                return Err(last_log_line(&base.join("server.log")).map_or_else(
                    || "OpenMausBot stopped while starting; see server.log".into(),
                    |line| format!("OpenMausBot stopped while starting: {line}"),
                ));
            }
            if Instant::now() > deadline {
                return Err("OpenMausBot did not answer in time; see server.log".into());
            }
            thread::sleep(Duration::from_millis(500));
        }
        let bridge = bridge::start(port).map_err(|_| "Cannot start the OpenMausBot bridge")?;
        let (bridge_port, token) = (bridge.port, bridge.token.clone());
        if let Some(running) = self.process.lock().unwrap().as_mut() {
            running._bridge = Some(bridge);
        }
        *self.status.lock().unwrap() = Status {
            port: Some(bridge_port),
            token: Some(token),
            ..Status::new("ready", "OpenMausBot is running")
        };
        loop {
            thread::sleep(Duration::from_secs(2));
            if self.process.lock().unwrap().is_none() {
                return Ok(());
            }
            if self.exited() {
                self.process.lock().unwrap().take();
                self.set("error", "OpenMausBot stopped; see server.log");
                return Ok(());
            }
        }
    }

    fn exited(&self) -> bool {
        match self.process.lock().unwrap().as_mut() {
            Some(running) => !matches!(running.child.try_wait(), Ok(None)),
            None => true,
        }
    }

    fn run_install_step(&self, mut command: Command, base: &Path) -> Result<(), String> {
        let log = fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(base.join("install.log"))
            .map_err(|_| "Cannot open install log")?;
        let errors = log.try_clone().map_err(|_| "Cannot open install log")?;
        command.stdin(Stdio::null()).stdout(log).stderr(errors);
        own_process_group(&mut command);
        *self.installer.lock().unwrap() = Some(
            command
                .spawn()
                .map_err(|_| "Cannot run npm; see install.log")?,
        );
        loop {
            let outcome = {
                let mut installer = self.installer.lock().unwrap();
                match installer.as_mut() {
                    None => return Err("OpenMausBot installation interrupted".into()),
                    Some(child) => match child.try_wait() {
                        Ok(Some(exit)) => {
                            *installer = None;
                            Some(exit)
                        }
                        Ok(None) => None,
                        Err(_) => return Err("Cannot monitor the installation".into()),
                    },
                }
            };
            if let Some(exit) = outcome {
                return if exit.success() {
                    Ok(())
                } else {
                    Err("OpenMausBot installation failed; see install.log".into())
                };
            }
            thread::sleep(Duration::from_millis(500));
        }
    }

    /// Ends the install and the server with everything they started: the
    /// bots' engines, shells and browsers live in the server's process group.
    pub fn stop(&self) {
        if let Some(child) = self.installer.lock().unwrap().take() {
            end_group(child);
        }
        if let Some(running) = self.process.lock().unwrap().take() {
            end_group(running.child);
        }
    }
}

impl Drop for Supervisor {
    fn drop(&mut self) {
        self.stop();
    }
}

fn own_process_group(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
        #[cfg(target_os = "linux")]
        unsafe {
            command.pre_exec(|| {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGTERM) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    #[cfg(not(unix))]
    let _ = command;
}

/// SIGTERM to the group, SIGKILL after a grace time (Windows: `taskkill /T`).
fn end_group(mut child: Child) {
    #[cfg(unix)]
    unsafe {
        libc::killpg(child.id() as i32, libc::SIGTERM);
        for _ in 0..50 {
            if matches!(child.try_wait(), Ok(Some(_))) {
                break;
            }
            thread::sleep(Duration::from_millis(100));
        }
        libc::killpg(child.id() as i32, libc::SIGKILL);
    }
    #[cfg(windows)]
    {
        let _ = Command::new("taskkill")
            .args(["/F", "/T", "/PID", &child.id().to_string()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
    let _ = child.kill();
    let _ = child.wait();
}

fn free_port() -> Result<u16, String> {
    TcpListener::bind(("127.0.0.1", 0))
        .and_then(|listener| listener.local_addr())
        .map(|address| address.port())
        .map_err(|_| "No free loopback port".into())
}

/// The server's last words, which name what it refused (a way in it
/// cannot open, a port in use).
fn last_log_line(path: &Path) -> Option<String> {
    let text = fs::read_to_string(path).ok()?;
    text.lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())
        .map(|line| line.chars().take(400).collect())
}

/// The operator's port, checked free where the server will listen.
fn fixed_port(port: u16, lan: bool) -> Result<u16, String> {
    let host = if lan { "0.0.0.0" } else { "127.0.0.1" };
    TcpListener::bind((host, port))
        .map(|_| port)
        .map_err(|_| format!("Port {port} is in use; choose another in Phone access"))
}

/// `GET /api/health` answers and names OpenMausBot.
fn healthy(port: u16) -> bool {
    let Ok(mut stream) =
        TcpStream::connect_timeout(&([127, 0, 0, 1], port).into(), Duration::from_secs(1))
    else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(4)));
    let request =
        format!("GET /api/health HTTP/1.1\r\nhost: 127.0.0.1:{port}\r\nconnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).is_err() {
        return false;
    }
    let mut reply = String::new();
    let _ = stream.take(64 * 1024).read_to_string(&mut reply);
    reply.starts_with("HTTP/1.1 200") && reply.contains("openmausbot")
}

fn private_directory(path: &Path) -> Result<(), String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .map_err(|_| "Cannot secure OpenMausBot directory")?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const SECOND: Duration = Duration::from_secs(1);

    #[test]
    fn progress_never_goes_back_or_reaches_a_step_end_early() {
        assert_eq!(progress_at(Step::Node, Duration::ZERO, Some(0.5)), 19);
        let mut last = 0;
        for seconds in 0..600 {
            let now = progress_at(Step::Packages, SECOND * seconds, None);
            assert!(now >= last);
            last = now;
        }
        assert!(last < 95);
    }

    #[test]
    fn status_never_leaks_the_bridge_token() {
        let status = Status {
            port: Some(42),
            token: Some("secret-secret-secret".into()),
            ..Status::new("ready", "ok")
        };
        assert!(!status.public().to_string().contains("secret"));
        assert_eq!(status.upstream()["token"], "secret-secret-secret");
        assert_eq!(
            Status::new("installing", "x").upstream()["status"],
            "starting"
        );
        assert_eq!(Status::new("error", "boom").upstream()["error"], "boom");
    }

    #[test]
    fn an_accepted_launch_leaves_the_error_at_once() {
        let supervisor = Supervisor::new();
        supervisor.set("error", "download interrupted");
        assert_eq!(supervisor.begin_launch(), Some(true));
        assert_eq!(supervisor.status().phase, "starting");
        assert_eq!(supervisor.begin_launch(), None);
    }
}
