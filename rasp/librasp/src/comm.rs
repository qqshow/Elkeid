use std::collections::HashMap;
use std::process::{ChildStdin, ChildStdout, Stdio};
// use std::fmt::{Display, Formatter, Result as FmtResult};
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Weak};
use std::thread;
use std::time::Duration;

use crossbeam::channel::{bounded, Receiver, SendError, Sender};
use libc::{kill, killpg, SIGKILL};
use log::*;

// use super::process::ProcessInfo;
use crate::async_command::run_async_process;
use crate::settings;
use anyhow::{anyhow, Result as AnyhowResult};

// https://stackoverflow.com/questions/35883390/how-to-check-if-a-thread-has-finished-in-rust
// https://stackoverflow.com/a/39615208
#[derive(Clone)]
pub struct Control {
    pub working_atomic: Arc<AtomicBool>,
    pub control: Weak<AtomicBool>,
}

impl Control {
    pub fn new() -> Self {
        let working = Arc::new(AtomicBool::new(true));
        let control = Arc::downgrade(&working);
        Control {
            working_atomic: working,
            control,
        }
    }
    pub fn check(&mut self) -> bool {
        (*self.working_atomic).load(Ordering::Relaxed)
    }
    pub fn stop(&mut self) -> Result<(), ()> {
        return match self.control.upgrade() {
            Some(working) => {
                (*working).store(false, Ordering::Relaxed);
                Ok(())
            }
            None => {
                // world stopped
                Err(())
            }
        };
    }
}

pub trait RASPComm {
    fn start_comm(
        &mut self,
        pid: i32,
        mnt_namespace: &String,
        probe_report_sender: Sender<plugins::Record>,
        patch_filed: HashMap<&'static str, String>,
    ) -> AnyhowResult<()>;
    fn stop_comm(&mut self, pid: i32, mnt_namespace: &String) -> AnyhowResult<()>;
    fn send_message_to_probe(
        &mut self,
        pid: i32,
        mnt_namespace: &String,
        message: &String,
    ) -> AnyhowResult<()>;
}

pub struct ThreadMode {
    pub ctrl: Control,
    pub log_level: String,
    pub bind_path: String,
    pub linking_to: Option<String>,
    pub using_mount: bool,
    pub agent_to_probe_sender: Sender<(i32, String)>,
}

impl ThreadMode {
    pub fn new(
        log_level: String,
        ctrl: Control,
        probe_report_sender: Sender<plugins::Record>,
        bind_path: String,
        linking_to: Option<String>,
        using_mount: bool,
    ) -> AnyhowResult<Self> {
        let (sender, receiver) = bounded(50);
        libraspserver::thread_mode::start(
            bind_path.clone(),
            20,
            libraspserver::utils::Control {
                working_atomic: ctrl.working_atomic.clone(),
                control: ctrl.control.clone(),
            },
            probe_report_sender,
            receiver,
        );
        Ok(Self {
            ctrl,
            log_level,
            bind_path: bind_path,
            linking_to: linking_to,
            using_mount,
            agent_to_probe_sender: sender,
        })
    }
}

pub struct ProcessMode {
    pub ctrl: Control,
    pub log_level: String,
    pub mnt_namesapce_server_map: HashMap<String, libraspserver::process_mode::RASPServerProcess>,
    pub mnt_namespace_comm_pair: HashMap<String, (Sender<String>, Receiver<String>)>,
}

impl ProcessMode {
    pub fn new(log_level: String, ctrl: Control) -> Self {
        Self {
            ctrl,
            log_level,
            mnt_namesapce_server_map: HashMap::new(),
            mnt_namespace_comm_pair: HashMap::new(),
        }
    }
}

impl RASPComm for ProcessMode {
    fn start_comm(
        &mut self,
        pid: i32,
        mnt_namespace: &String,
        probe_report_sender: Sender<plugins::Record>,
        patch_field: HashMap<&'static str, String>,
    ) -> AnyhowResult<()> {
        let (probe_mesasge_sender, probe_message_receiver) = bounded(50);
        let mut server_process = libraspserver::process_mode::RASPServerProcess::new(
            pid,
            probe_report_sender,
            probe_message_receiver.clone(),
            self.log_level.clone(),
            patch_field,
            libraspserver::utils::Control {
                working_atomic: self.ctrl.working_atomic.clone(),
                control: self.ctrl.control.clone(),
            },
        )?;
        server_process.spawn(settings::RASP_SERVER_BIN().as_str())?;
        self.mnt_namesapce_server_map
            .insert(mnt_namespace.clone(), server_process);
        self.mnt_namespace_comm_pair.insert(
            mnt_namespace.clone(),
            (probe_mesasge_sender, probe_message_receiver),
        );
        Ok(())
    }

    fn stop_comm(&mut self, _pid: i32, mnt_namespace: &String) -> AnyhowResult<()> {
        info!("stop server: {}", mnt_namespace.clone());
        return if let Some(mut runner) = self.mnt_namesapce_server_map.remove(mnt_namespace) {
            runner.kill();
            Ok(())
        } else {
            Err(anyhow!(
                "didn't start server for mnt namespace: {}",
                mnt_namespace.clone()
            ))
        };
    }
    fn send_message_to_probe(
        &mut self,
        _pid: i32,
        mnt_namespace: &String,
        message: &String,
    ) -> AnyhowResult<()> {
        if let Some(p) = self.mnt_namespace_comm_pair.get(mnt_namespace) {
            if let Err(e) = p.0.send(message.clone()) {
                return Err(anyhow!("send to probe failed: {}", e.to_string()));
            }
        }
        Ok(())
    }
}

impl RASPComm for ThreadMode {
    fn start_comm(
        &mut self,
        pid: i32,
        _mnt_namespace: &String,
        _probe_report_sender: Sender<plugins::Record>,
        _patch_filed: HashMap<&'static str, String>,
    ) -> AnyhowResult<()> {
        match check_need_mount(_mnt_namespace) {
            Ok(same_ns) => {
                if same_ns{
                    self.using_mount = false;
                    info!(
                        "process {} namespace as same as root, so no need to mount, using_mount : {}", pid, self.using_mount
                    );
                } else {
                    self.using_mount = true;
                    info!(
                        "process {} namespace are not same as root, so need to mount", pid
                    );
                }
            }
            Err(e) => {
                warn!(
                    "check_need_mount failed, {}", e
                );
            }
        }
        if self.using_mount {
            if let Some(bind_dir) = std::path::Path::new(&self.bind_path.clone()).parent() {
                let bind_dir_str = bind_dir.to_str().unwrap();
                mount(pid, bind_dir_str, bind_dir_str)?
            }
        }
        if let Some(linking_to) = self.linking_to.clone() {
            match std::process::Command::new(settings::RASP_NS_ENTER_BIN())
                .args([
                    "-t",
                    pid.to_string().as_str(),
                    "-m",
                    "-i",
                    "-n",
                    "-p",
                    "/bin/ln",
                    "-sf",
                    self.bind_path.as_str(),
                    linking_to.as_str(),
                ])
                .output()
            {
                Ok(o) => {
                    info!("LN {} {:?} {:?}", o.status, o.stdout, o.stderr);
                }
                Err(e) => {
                    error!("LN can not run: {}", e);
                    return Err(anyhow!("link bind path failed: {}", e));
                }
            };
        }
        Ok(())
    }
    fn stop_comm(&mut self, _pid: i32, _mnt_namespace: &String) -> AnyhowResult<()> {
        Ok(())
    }
    fn send_message_to_probe(
        &mut self,
        pid: i32,
        _mnt_namespace: &String,
        message: &String,
    ) -> AnyhowResult<()> {
        debug!("recv thread mode message: {}", message);
        match self.agent_to_probe_sender.send((pid, message.clone())) {
            Ok(_) => {
                debug!("sending to probe: {} {}", pid, message.clone());
            }
            Err(SendError(e)) => {
                error!("send error: {:?}", e);
                let _ = self.ctrl.stop();
                return Err(anyhow!("send message to probe failed: {} {}", e.0, e.1));
            }
        }
        Ok(())
    }
}

fn mount(pid: i32, from: &str, to: &str) -> AnyhowResult<()> {
    let pid_str = pid.to_string();
    let nsenter_str = settings::RASP_NS_ENTER_BIN();
    let args = [pid_str.as_str(), from, to, nsenter_str.as_str()];
    return match run_async_process(
        std::process::Command::new(settings::RASP_MOUNT_SCRIPT_BIN()).args(args),
    ) {
        Ok((exit_status, stdout, stderr)) => {
            if !exit_status.success() {
                error!(
                    "mount script execute failed: {} {} {}",
                    exit_status, stdout, stderr
                );
                return Err(anyhow!(
                    "mount script execute failed: {} {} {} ",
                    exit_status,
                    stdout,
                    stderr
                ));
            }
            debug!("mount success: {} {} {}", exit_status, stdout, stderr);
            Ok(())
        }
        Err(e) => Err(anyhow!("can not mount: {}", e)),
    };
}

fn check_need_mount(pid_mntns: &String) -> AnyhowResult<bool> {
    let root_mnt = std::fs::read_link("/proc/1/ns/mnt")?;
    debug!(
        "pid namespace && root namespace : {} && {}",
        pid_mntns, root_mnt.display()
    );
    Ok(&root_mnt.display().to_string() == pid_mntns)
}

pub struct EbpfMode {
    pub ctrl: Control,
    pub kernel_version: procfs::sys::kernel::Version,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
}

impl EbpfMode {
    pub fn new(ctrl: Control) -> AnyhowResult<Self> {
        let ebpf_manager = Self {
            ctrl,
            kernel_version: Self::detect_kernel_version()?,
            stdin: None,
            stdout: None,
        };
        let _ = ebpf_manager.switch_bpf_main_process()?;
        Ok(ebpf_manager)
    }
    pub fn detect_kernel_version() -> AnyhowResult<procfs::sys::kernel::Version> {
        let kernel_version = procfs::sys::kernel::Version::current()?;
        info!(
            "current kernel version: {}.{}",
            kernel_version.major, kernel_version.minor
        );
        Ok(kernel_version)
    }
    pub fn switch_bpf_main_process(&self) -> AnyhowResult<String> {
        /*
        [4.14, 4.16) minimal support
        [4.16, 5.2) http support(without header)
        [5.2,  5.8) http support(with header)
        [5.8,  current) http support(with header), ring buffer support
        */
        let bpf_process_version =
            if self.kernel_version >= procfs::sys::kernel::Version::new(5, 8, 0) {
                "_5.8"
            } else if self.kernel_version >= procfs::sys::kernel::Version::new(5, 2, 0) {
                "_5.2"
            } else if self.kernel_version >= procfs::sys::kernel::Version::new(4, 16, 0) {
                "_4.16"
            } else if self.kernel_version >= procfs::sys::kernel::Version::new(4, 14, 0) {
                "_4.14"
            } else {
                return Err(anyhow!(
                    "version: {}.{} kernel not support",
                    self.kernel_version.major,
                    self.kernel_version.minor,
                ));
            };
        return Ok(bpf_process_version.to_string());
    }
    pub fn start_server(&mut self) -> AnyhowResult<()> {
        let bin_path = settings::RASP_GOLANG_EBPF(&self.switch_bpf_main_process()?);
        let mut child = std::process::Command::new(bin_path)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        debug!("spawn ebpf process success: {}", child.id());
        let child_id = child.id();
        self.stdin = child.stdin.take();
        self.stdout = child.stdout.take();
        /*
            if self.stdin.is_none() {
                return Err(anyhow!("can not take child stdin, pid: {}", child_id));
            }
            if self.stdout.is_none() {
                return Err(anyhow!("can not take child stdout, pid: {}", child_id));
            }
        */
        // start a thread for wait child die
        let mut wait_ctrl = self.ctrl.clone();
        thread::Builder::new()
            .name("ebpf_server_wait".to_string())
            .spawn(move || loop {
                if !wait_ctrl.check() {
                    Self::kill_server(child_id as i32);
                    return;
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        info!("Golang EBPF daemon exit with status: {}", status);
                        return;
                    }
                    Ok(None) => {
			thread::sleep(Duration::from_secs(10));
		    }
                    Err(e) => {
                        error!("error attempting to wait: {}", e);
                        Self::kill_server(child_id as i32);
                        return;
                    }
                }
            })?;
        // sleep here for subprocess ready for listen stdin
        thread::sleep(Duration::from_secs(2));
        Ok(())
    }
    pub fn attach(&mut self, pid: i32) -> AnyhowResult<bool> {
        self.write_stdin(pid)?;
        match self.read_stdout(pid) {
            Ok(result) => {
                if !result.is_empty() {
                    return Ok(false);
                }
            }
            Err(e) => {
                error!("ebpf running abnormally: {}, quiting.", e);
                let _ = self.ctrl.stop();
                return Err(e);
            }
        }
        Ok(true)
    }
    pub fn write_stdin(&mut self, pid: i32) -> AnyhowResult<()> {
        let mut stdin = self.stdin.as_ref().unwrap();
        stdin.write_all(format!("{}\n", pid).as_bytes())?;
        stdin.flush()?;
        Ok(())
    }
    pub fn read_stdout(&mut self, pid: i32) -> AnyhowResult<String> {
        let mut buf_reader = if let Some(stdout) = self.stdout.take() {
            BufReader::new(stdout)
        } else {
            return Err(anyhow!(""));
        };
        let mut times = 10;
        let interval = 1; // second
        loop {
            times -= 1;
            if times <= 0 {
                return Err(anyhow!("read stdout from ebpf server timeout: {}", pid));
            }
            if buf_reader.fill_buf()?.len() <= 0 {
                std::thread::sleep(Duration::from_secs(interval));
                continue;
            }
            let mut read_from_server = String::new();
            let size = buf_reader.read_line(&mut read_from_server)?;
            if size == 0 {
                return Err(anyhow!("read stdout from ebpf server EOF"));
            }
            let (pid_from_server, success) = Self::parse_server_response(&read_from_server)?;
            if pid_from_server != pid {
                return Err(anyhow!(
                    "pid miss match: expect: {} response: {}",
                    pid,
                    pid_from_server
                ));
            }
            if success {
                return Ok(String::new());
            } else {
                return Ok(format!("target pid: {} attach failed", pid));
            }
        }
    }
    pub fn kill_server(pid: i32) {
        unsafe {
            killpg(pid, SIGKILL);
            kill(pid as i32, SIGKILL);
        }
    }
    pub fn parse_server_response(response: &String) -> AnyhowResult<(i32, bool)> {
        let regex = regex::Regex::new(r"(\d{1,20}):(succeed|failed)")?;
        if let Some(caps) = regex.captures(response) {
            if caps.len() != 3 {
                return Err(anyhow!("response format can not parse: {}", response));
            }
            // pid
            let pid: i32 = if let Some(pid) = caps.get(1) {
                pid.as_str().parse()?
            } else {
                return Err(anyhow!("response format can not parse: {}", response));
            };
            let result = if let Some(result) = caps.get(2) {
                match result.as_str() {
                    "succeed" => true,
                    "failed" => false,
                    _ => {
                        return Err(anyhow!("response format can not parse: {}", response));
                    }
                }
            } else {
                return Err(anyhow!("response format can not parse: {}", response));
            };
            return Ok((pid, result));
        }
        return Err(anyhow!(
            "can not found any proper format in response: {}",
            response
        ));
    }
}
