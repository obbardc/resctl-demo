// Copyright (c) Facebook, Inc. and its affiliates.
#![allow(dead_code)]
use anyhow::{anyhow, bail, Result};
use log::{debug, error, warn};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread::{spawn, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use util::*;

use super::progress::BenchProgress;
use super::{rd_agent_base_args, AGENT_BIN};
use rd_agent_intf::{
    AgentFiles, ReportIter, RunnerState, Slice, AGENT_SVC_NAME, HASHD_BENCH_SVC_NAME,
};

const MINDER_AGENT_TIMEOUT: Duration = Duration::from_secs(30);
const CMD_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinderState {
    Ok,
    AgentTimeout,
    AgentNotRunning(systemd::UnitState),
    ReportTimeout,
}

struct RunCtxInner {
    dir: String,
    dev: Option<String>,
    linux_tar: Option<String>,
    need_linux_tar: bool,
    prep_testfiles: bool,
    bypass: bool,
    passive_all: bool,
    passive_keep_crit_mem_prot: bool,

    agent_files: AgentFiles,
    agent_svc: Option<TransientService>,
    minder_state: MinderState,
    minder_jh: Option<JoinHandle<()>>,
}

impl RunCtxInner {
    fn start_agent_svc(&self, mut extra_args: Vec<String>) -> Result<TransientService> {
        let mut args = vec![AGENT_BIN.clone()];
        args.append(&mut rd_agent_base_args(&self.dir, self.dev.as_deref())?);
        args.push("--reset".into());
        args.push("--keep-reports".into());

        if self.need_linux_tar {
            if self.linux_tar.is_some() {
                args.push("--linux-tar".into());
                args.push(self.linux_tar.as_ref().unwrap().into());
            }
        } else {
            args.push("--linux-tar".into());
            args.push("__SKIP__".into());
        }

        if self.bypass {
            args.push("--bypass".into());
        }

        if self.passive_all {
            args.push("--passive=all".into());
        } else if self.passive_keep_crit_mem_prot {
            args.push("--passive=keep-crit-mem-prot".into());
        }

        args.append(&mut extra_args);

        let mut svc =
            TransientService::new_sys(AGENT_SVC_NAME.into(), args, Vec::new(), Some(0o002))?;
        svc.set_slice(Slice::Host.name()).set_quiet();
        svc.start()?;

        Ok(svc)
    }

    fn start_agent(&mut self, extra_args: Vec<String>) -> Result<()> {
        if self.agent_svc.is_some() {
            bail!("already running");
        }

        // prepare testfiles synchronously for better progress report
        if self.prep_testfiles {
            let hashd_bin =
                find_bin("rd-hashd", exe_dir().ok()).ok_or(anyhow!("can't find rd-hashd"))?;
            let testfiles_path = self.dir.clone() + "/scratch/hashd-A/testfiles";

            let status = Command::new(&hashd_bin)
                .arg("--testfiles")
                .arg(testfiles_path)
                .arg("--keep-caches")
                .arg("--prepare")
                .status()?;
            if !status.success() {
                bail!("failed to prepare testfiles ({})", &status);
            }
        }

        // start agent
        let svc = self.start_agent_svc(extra_args)?;
        self.agent_svc.replace(svc);

        Ok(())
    }
}

pub struct RunCtx {
    inner: Arc<Mutex<RunCtxInner>>,
}

impl RunCtx {
    pub fn new(dir: &str, dev: Option<&str>, linux_tar: Option<&str>) -> Self {
        Self {
            inner: Arc::new(Mutex::new(RunCtxInner {
                dir: dir.into(),
                dev: dev.map(Into::into),
                linux_tar: linux_tar.map(Into::into),
                need_linux_tar: false,
                prep_testfiles: false,
                bypass: false,
                passive_all: false,
                passive_keep_crit_mem_prot: false,
                agent_files: AgentFiles::new(dir),
                agent_svc: None,
                minder_state: MinderState::Ok,
                minder_jh: None,
            })),
        }
    }

    pub fn set_need_linux_tar(&self) -> &Self {
        self.inner.lock().unwrap().need_linux_tar = true;
        self
    }

    pub fn set_prep_testfiles(&self) -> &Self {
        self.inner.lock().unwrap().prep_testfiles = true;
        self
    }

    pub fn set_bypass(&self) -> &Self {
        self.inner.lock().unwrap().bypass = true;
        self
    }

    pub fn set_passive_all(&self) -> &Self {
        self.inner.lock().unwrap().passive_all = true;
        self
    }

    pub fn set_passive_keep_crit_mem_prot(&self) -> &Self {
        self.inner.lock().unwrap().passive_keep_crit_mem_prot = true;
        self
    }

    fn minder(inner: Arc<Mutex<RunCtxInner>>) {
        let mut last_status_at = SystemTime::now();
        let mut last_report_at = SystemTime::now();
        let mut next_at = unix_now() + 1;

        'outer: loop {
            let sleep_till = UNIX_EPOCH + Duration::from_secs(next_at);
            'sleep: loop {
                match sleep_till.duration_since(SystemTime::now()) {
                    Ok(dur) => {
                        if wait_prog_state(dur) == ProgState::Exiting {
                            break 'outer;
                        }
                    }
                    _ => break 'sleep,
                }
            }
            next_at = unix_now() + 1;

            let mut ctx = inner.lock().unwrap();

            let svc = match ctx.agent_svc.as_mut() {
                Some(v) => v,
                None => {
                    debug!("minder: agent_svc is None, exiting");
                    break 'outer;
                }
            };

            let mut nr_tries = 3;
            'status: loop {
                if let Err(e) = svc.unit.refresh() {
                    if SystemTime::now().duration_since(last_status_at).unwrap()
                        > MINDER_AGENT_TIMEOUT
                    {
                        error!(
                            "minder: failed to update agent status for over {}s, giving up ({})",
                            MINDER_AGENT_TIMEOUT.as_secs(),
                            &e
                        );
                        ctx.minder_state = MinderState::AgentTimeout;
                        break 'outer;
                    }
                    warn!("minder: failed to refresh agent status ({})", &e);
                }
                last_status_at = SystemTime::now();

                if svc.unit.state != systemd::UnitState::Running {
                    if nr_tries > 0 {
                        warn!(
                            "minder: agent status != running ({:?}), re-verifying...",
                            &svc.unit.state
                        );
                        nr_tries -= 1;
                        continue 'status;
                    } else {
                        error!("minder: agent is not running ({:?})", &svc.unit.state);
                        ctx.minder_state = MinderState::AgentNotRunning(svc.unit.state.clone());
                        break 'outer;
                    }
                }

                break 'status;
            }

            ctx.agent_files.refresh();
            prog_kick();

            let report_at = SystemTime::from(ctx.agent_files.report.data.timestamp);
            if report_at > last_report_at {
                last_report_at = report_at;
            }

            match SystemTime::now().duration_since(last_report_at) {
                Ok(dur) if dur > MINDER_AGENT_TIMEOUT => {
                    error!(
                        "minder: agent report is older than {}s, giving up",
                        MINDER_AGENT_TIMEOUT.as_secs()
                    );
                    ctx.minder_state = MinderState::ReportTimeout;
                    break 'outer;
                }
                _ => (),
            }
        }

        inner.lock().unwrap().agent_files.refresh();
        prog_kick();
    }

    pub fn start_agent_fallible(&self, extra_args: Vec<String>) -> Result<()> {
        let mut ctx = self.inner.lock().unwrap();

        ctx.start_agent(extra_args)?;

        // start minder and wait for the agent to become Running
        let inner = self.inner.clone();
        ctx.minder_jh = Some(spawn(move || Self::minder(inner)));

        drop(ctx);

        let started_at = unix_now() as i64;
        if let Err(e) = self.wait_cond_fallible(
            |af, _| {
                af.report.data.timestamp.timestamp() >= started_at
                    && af.report.data.state == RunnerState::Running
            },
            Some(Duration::from_secs(30)),
            None,
        ) {
            self.stop_agent();
            bail!("rd-agent failed to report back after startup ({})", &e);
        }

        Ok(())
    }

    pub fn start_agent(&self) {
        if let Err(e) = self.start_agent_fallible(vec![]) {
            error!("Failed to start rd-agent ({})", &e);
            panic!();
        }
    }

    pub fn stop_agent(&self) {
        let agent_svc = self.inner.lock().unwrap().agent_svc.take();
        if let Some(svc) = agent_svc {
            drop(svc);
        }

        prog_kick();

        let minder_jh = self.inner.lock().unwrap().minder_jh.take();
        if let Some(jh) = minder_jh {
            jh.join().unwrap();
        }
    }

    pub fn wait_cond_fallible<F>(
        &self,
        mut cond: F,
        timeout: Option<Duration>,
        progress: Option<BenchProgress>,
    ) -> Result<()>
    where
        F: FnMut(&AgentFiles, &mut BenchProgress) -> bool,
    {
        let timeout = match timeout {
            Some(v) => v,
            None => Duration::from_secs(365 * 24 * 3600),
        };
        let expires = SystemTime::now() + timeout;
        let mut progress = match progress {
            Some(v) => v,
            None => BenchProgress::new(),
        };

        loop {
            let ctx = self.inner.lock().unwrap();
            if cond(&ctx.agent_files, &mut progress) {
                return Ok(());
            }
            if ctx.minder_state != MinderState::Ok {
                bail!("agent error ({:?})", ctx.minder_state);
            }
            drop(ctx);

            let dur = match expires.duration_since(SystemTime::now()) {
                Ok(v) => v,
                _ => bail!("timeout"),
            };
            if wait_prog_state(dur) == ProgState::Exiting {
                bail!("exiting");
            }
        }
    }

    pub fn wait_cond<F>(&self, cond: F, timeout: Option<Duration>, progress: Option<BenchProgress>)
    where
        F: FnMut(&AgentFiles, &mut BenchProgress) -> bool,
    {
        if let Err(e) = self.wait_cond_fallible(cond, timeout, progress) {
            error!("Failed to wait for condition ({})", &e);
            panic!();
        }
    }

    pub fn access_agent_files<F, T>(&self, func: F) -> T
    where
        F: FnOnce(&mut AgentFiles) -> T,
    {
        let mut ctx = self.inner.lock().unwrap();
        let af = &mut ctx.agent_files;
        func(af)
    }

    pub fn start_hashd_bench(&self, ballon_size: usize, log_bps: u64, extra_args: Vec<String>) {
        debug!("Starting hashd benchmark ({})", &HASHD_BENCH_SVC_NAME);

        let mut next_seq = 0;
        self.access_agent_files(|af| {
            next_seq = af.bench.data.hashd_seq + 1;
            af.cmd.data = Default::default();
            af.cmd.data.hashd[0].log_bps = log_bps;
            af.cmd.data.bench_hashd_balloon_size = ballon_size;
            af.cmd.data.bench_hashd_args = extra_args;
            af.cmd.data.bench_hashd_seq = next_seq;
            af.cmd.save().unwrap();
        });

        self.wait_cond(
            |af, _| {
                af.report.data.state == RunnerState::BenchHashd
                    || af.bench.data.hashd_seq >= next_seq
            },
            Some(CMD_TIMEOUT),
            None,
        );
    }

    pub const BENCH_FAKE_CPU_HASH_SIZE: usize = 5 << 20;
    pub const BENCH_FAKE_CPU_RPS_MAX: u32 = 1000;
    pub const BENCH_FAKE_CPU_LOG_BPS: u64 = 16 << 20;

    pub fn start_hashd_fake_cpu_bench(
        &self,
        balloon_size: usize,
        log_bps: u64,
        hash_size: usize,
        rps_max: u32,
    ) {
        self.start_hashd_bench(
            balloon_size,
            log_bps,
            vec![
                "--bench-fake-cpu-load".into(),
                format!("--bench-hash-size={}", hash_size),
                format!("--bench-rps-max={}", rps_max),
            ],
        );
    }

    pub fn report_iter(&self, start: u64, end: u64) -> ReportIter {
        let ctx = self.inner.lock().unwrap();
        ReportIter::new(&ctx.agent_files.index.data.report_d, start, end)
    }
}

impl Drop for RunCtx {
    fn drop(&mut self) {
        self.stop_agent();
    }
}
