// Copyright (c) Facebook, Inc. and its affiliates.
use anyhow::Result;
use enum_iterator::IntoEnumIterator;
use glob::glob;
use log::{debug, error, info, trace, warn};
use scan_fmt::scan_fmt;
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::fmt::Write;
use std::fs;
use std::io::prelude::*;
use std::path::Path;
use util::systemd::UnitState as US;
use util::*;

use super::Config;
use rd_agent_intf::{
    DisableSeqKnobs, EnforceConfig, MemoryKnob, Slice, SliceConfig, SliceKnobs, SysReq,
};

pub fn check_other_io_controllers(sr_failed: &mut BTreeSet<SysReq>) {
    let mut failed = None;
    let mut nr_fails = 0;

    for path in glob("/sys/fs/cgroup/**/io.latency")
        .unwrap()
        .chain(glob("/sys/fs/cgroup/**/io.max").unwrap())
        .chain(glob("/sys/fs/cgroup/**/io.low").unwrap())
        .filter_map(Result::ok)
    {
        match read_one_line(&path) {
            Ok(line) if line.trim().len() == 0 => continue,
            Err(_) => continue,
            _ => {}
        }
        if failed.is_none() {
            failed = path
                .parent()
                .and_then(|x| x.file_name())
                .and_then(|x| Some(x.to_string_lossy().into_owned()));
            sr_failed.insert(SysReq::NoOtherIoControllers);
        }
        nr_fails += 1;
    }

    if let Some(failed) = failed {
        error!(
            "resctl: {} cgroups including {:?} have non-empty io.latency/low/max configs: disable",
            nr_fails, &failed
        );
    }
}

fn mknob_to_cgrp_string(knob: &MemoryKnob, is_limit: bool) -> String {
    match knob.nr_bytes(is_limit) {
        std::u64::MAX => "max".to_string(),
        v => format!("{}", v),
    }
}

fn mknob_to_systemd_string(knob: &MemoryKnob, is_limit: bool) -> String {
    match knob.nr_bytes(is_limit) {
        std::u64::MAX => "infinity".to_string(),
        v => format!("{}", v),
    }
}

fn mknob_to_unit_resctl(knob: &MemoryKnob) -> Option<u64> {
    match knob {
        MemoryKnob::None => None,
        _ => Some(knob.nr_bytes(true)),
    }
}

fn slice_needs_mem_prot_propagation(slice: Slice) -> bool {
    match slice {
        Slice::Work | Slice::Side => false,
        _ => true,
    }
}

fn slice_needs_start_stop(slice: Slice) -> bool {
    match slice {
        Slice::Side => true,
        _ => false,
    }
}

fn slice_needs_crit_mem_prot(slice: Slice) -> bool {
    match slice {
        Slice::Host | Slice::Init => true,
        _ => false,
    }
}

fn slice_enforce_mem(ecfg: &EnforceConfig, slice: Slice) -> bool {
    ecfg.mem || (ecfg.crit_mem_prot && slice_needs_crit_mem_prot(slice))
}

fn build_configlet(
    slice: Slice,
    cpu_weight: Option<u32>,
    io_weight: Option<u32>,
    mem_min: Option<MemoryKnob>,
    mem_low: Option<MemoryKnob>,
    mem_high: Option<MemoryKnob>,
) -> String {
    let section = if slice.name().ends_with(".slice") {
        "Slice"
    } else {
        "Scope"
    };

    let mut buf = format!(
        "# Generated by rd-agent. Do not edit directly.\n\
         [{}]\n",
        section
    );

    if let Some(w) = cpu_weight {
        writeln!(buf, "CPUWeight={}", w).unwrap();
    }
    if let Some(w) = io_weight {
        writeln!(buf, "IOWeight={}", w).unwrap();
    }
    if let Some(m) = mem_min {
        writeln!(buf, "MemoryMin={}", mknob_to_systemd_string(&m, false)).unwrap();
    }
    if let Some(m) = mem_low {
        writeln!(buf, "MemoryLow={}", mknob_to_systemd_string(&m, false)).unwrap();
    }
    if let Some(m) = mem_high {
        writeln!(buf, "MemoryHigh={}", mknob_to_systemd_string(&m, true)).unwrap();
    }

    buf
}

fn apply_configlet(slice: Slice, configlet: &str) -> Result<bool> {
    let path = crate::unit_configlet_path(slice.name(), "resctl");

    debug!("resctl: reading {:?} to test for equality", &path);
    if let Ok(mut f) = fs::OpenOptions::new().read(true).open(&path) {
        let mut buf = String::new();
        f.read_to_string(&mut buf)?;
        if buf == configlet {
            debug!("resctl: {:?} doesn't need to change", &path);
            return Ok(false);
        }
    }

    debug!("resctl: writing updated {:?}", &path);
    crate::write_unit_configlet(slice.name(), "resctl", &configlet)?;

    if slice_needs_start_stop(slice) {
        match systemd::Unit::new_sys(slice.name().into()) {
            Ok(mut unit) => {
                if let Err(e) = unit.try_start_nowait() {
                    warn!("resctl: Failed to start {:?} ({})", slice.name(), &e);
                }
            }
            Err(e) => {
                warn!(
                    "resctl: Failed to create unit for {:?} ({})",
                    slice.name(),
                    &e
                );
            }
        }
    }

    Ok(true)
}

fn propagate_one_slice(slice: Slice, resctl: &systemd::UnitResCtl) -> Result<()> {
    debug!("resctl: propagating {:?} w/ {:?}", slice, &resctl);

    for path in glob(&format!("{}/**/*.service", slice.cgrp()))
        .unwrap()
        .chain(glob(&format!("{}/**/*.scope", slice.cgrp())).unwrap())
        .chain(glob(&format!("{}/**/*.slice", slice.cgrp())).unwrap())
        .filter_map(Result::ok)
    {
        let unit_name = path.file_name().unwrap().to_str().unwrap().to_string();
        let unit = systemd::Unit::new_sys(unit_name.clone());
        if let Err(e) = unit {
            debug!(
                "resctl: Failed to create {:?} for resctl config propagation ({:?})",
                &unit_name, &e
            );
            continue;
        }
        let mut unit = unit.unwrap();

        let trimmed = path
            .components()
            .skip(4)
            .fold(OsString::new(), |mut acc, x| {
                acc.push("/");
                acc.push(x);
                acc
            });
        match unit.props.string("ControlGroup") {
            Some(v) if AsRef::<OsStr>::as_ref(&v) == trimmed => {}
            v => {
                trace!("resctl: skipping {:?} != {:?}", &v, &trimmed);
                continue;
            }
        }

        match unit.state {
            US::Running | US::OtherActive(_) => {}
            _ => {
                trace!(
                    "resctl: skipping {:?} due to invalid state {:?}",
                    &unit_name,
                    unit.state
                );
                continue;
            }
        }

        if unit.resctl == *resctl {
            trace!("resctl: no change needed for {:?}", &unit_name);
            continue;
        }

        unit.resctl = resctl.clone();
        match unit.apply() {
            Ok(()) => debug!("resctl: propagated resctl config to {:?}", &unit_name),
            Err(e) => warn!(
                "resctl: Failed to propagate config to {:?} ({:?})",
                &unit_name, &e
            ),
        }
    }
    Ok(())
}

pub fn apply_slices(knobs: &mut SliceKnobs, hashd_mem_size: u64, cfg: &Config) -> Result<()> {
    if knobs.work_mem_low_none {
        let sk = knobs.slices.get_mut(Slice::Work.name()).unwrap();
        sk.mem_low = MemoryKnob::Bytes((hashd_mem_size as f64 * 0.75).ceil() as u64);
    }

    let mut updated = false;
    for slice in Slice::into_enum_iter() {
        let enforce_mem = slice_enforce_mem(&cfg.enforce, slice);

        if !cfg.enforce.cpu && !enforce_mem && !cfg.enforce.io {
            continue;
        }

        let sk = knobs.slices.get(slice.name()).unwrap();
        let (cpu_weight, io_weight, mem_min, mem_low, mem_high);

        cpu_weight = match cfg.enforce.cpu {
            true => Some(sk.cpu_weight),
            false => None,
        };
        io_weight = match cfg.enforce.io {
            true => Some(sk.io_weight),
            false => None,
        };

        if enforce_mem {
            mem_min = Some(sk.mem_min);
            mem_high = Some(sk.mem_high);
            if slice == Slice::Work && knobs.disable_seqs.mem >= super::instance_seq() {
                mem_low = None;
            } else {
                mem_low = Some(sk.mem_low);
            }
        } else {
            mem_min = None;
            mem_low = None;
            mem_high = None;
        }

        let configlet = build_configlet(slice, cpu_weight, io_weight, mem_min, mem_low, mem_high);
        if apply_configlet(slice, &configlet)? {
            updated = true;
        }

        if enforce_mem && slice_needs_mem_prot_propagation(slice) {
            let sk = knobs.slices.get(slice.name()).unwrap();
            let mut resctl = systemd::UnitResCtl::default();

            if !cfg.memcg_recursive_prot() {
                resctl.mem_min = mknob_to_unit_resctl(&sk.mem_min);
                resctl.mem_low = mknob_to_unit_resctl(&sk.mem_low);
            }

            propagate_one_slice(slice, &resctl)?;
        }
    }
    if updated {
        info!("resctl: Applying updated slice configurations");
        systemd::daemon_reload()?;
    }

    let enable_iocost = knobs.disable_seqs.io < super::instance_seq();
    if let Err(e) = super::bench::iocost_on_off(enable_iocost, cfg) {
        warn!("resctl: Failed to enable/disable iocost ({:?})", &e);
        return Err(e);
    }

    Ok(())
}

fn clear_one_slice(slice: Slice, ecfg: &EnforceConfig) -> Result<bool> {
    match systemd::Unit::new_sys(slice.name().into()) {
        Ok(mut unit) => {
            if ecfg.cpu {
                unit.resctl.cpu_weight = None;
            }
            if slice_enforce_mem(ecfg, slice) {
                unit.resctl.mem_min = None;
                unit.resctl.mem_low = None;
            }
            if ecfg.io {
                unit.resctl.io_weight = None;
            }
            if let Err(e) = unit.apply() {
                error!("resctl: Failed to reset {:?} ({})", slice.name(), &e);
            }
            if slice_needs_start_stop(slice) {
                if let Err(e) = unit.stop() {
                    error!("resctl: Failed to stop {:?} ({})", slice.name(), &e);
                }
            }
        }
        Err(e) => {
            error!(
                "resctl: Failed to clear unit for {:?} ({})",
                slice.name(),
                &e
            );
        }
    }

    let path = crate::unit_configlet_path(slice.name(), "resctl");
    if Path::new(&path).exists() {
        debug!("resctl: Removing {:?}", &path);
        fs::remove_file(&path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

pub fn clear_slices(ecfg: &EnforceConfig) -> Result<()> {
    let mut updated = false;
    for slice in Slice::into_enum_iter() {
        let enforce_mem = slice_enforce_mem(ecfg, slice);

        if !ecfg.cpu && !enforce_mem && !ecfg.io {
            continue;
        }

        match clear_one_slice(slice, &ecfg) {
            Ok(true) => updated = true,
            Ok(false) => {}
            Err(e) => warn!(
                "resctl: Failed to clear configurations for {:?} ({:?})",
                slice.name(),
                &e
            ),
        }

        if enforce_mem && slice_needs_mem_prot_propagation(slice) {
            propagate_one_slice(slice, &Default::default())?;
        }
    }
    if updated {
        systemd::daemon_reload()?;
    }
    Ok(())
}

fn fix_overrides(dseqs: &DisableSeqKnobs, cfg: &Config) -> Result<()> {
    let seq = super::instance_seq();
    let mut disable = String::new();
    let mut enable = String::new();

    if cfg.enforce.cpu {
        if dseqs.cpu < seq {
            enable += " +cpu";
        } else {
            disable += " -cpu";
        }
    }
    if cfg.enforce.io {
        enable += " +io";
    }

    if cfg.enforce.crit_mem_prot {
        enable += " +memory";
    }

    if disable.len() > 0 {
        let mut scs: Vec<String> = glob("/sys/fs/cgroup/**/cgroup.subtree_control")
            .unwrap()
            .filter_map(|x| x.ok())
            .map(|x| x.to_str().unwrap().to_string())
            .collect();
        scs.sort_unstable_by_key(|x| -(x.len() as i64));

        let mut nr_failed = 0;
        for sc in &scs {
            if let Err(e) = write_one_line(sc, &disable) {
                if nr_failed == 0 {
                    warn!(
                        "resctl: Failed to write {:?} to {:?} ({:?})",
                        &disable, &sc, &e
                    );
                }
                nr_failed += 1;
            }
        }

        if nr_failed > 1 {
            warn!(
                "resctl: Failed to write {:?} to {} files",
                &disable, nr_failed
            );
        }
    }

    if enable.len() > 0 {
        write_one_line("/sys/fs/cgroup/cgroup.subtree_control", &enable)?;
    }

    Ok(())
}

fn fix_slice_cpu(sk: &SliceConfig, path: &str, enable: bool) -> Result<()> {
    if !enable {
        return Ok(());
    }
    let cpu_weight_path = path.to_string() + "/cpu.weight";
    trace!("resctl: verify: {:?}", &cpu_weight_path);
    let line = read_one_line(&cpu_weight_path)?;
    match scan_fmt!(&line, "{d}", u32) {
        Ok(v) if v == sk.cpu_weight => {}
        v => {
            info!(
                "resctl: {:?} should be {} but is {:?}, fixing",
                &cpu_weight_path, sk.cpu_weight, &v
            );
            write_one_line(&cpu_weight_path, &format!("{}", sk.cpu_weight))?;
        }
    }
    Ok(())
}

fn fix_slice_io(sk: &SliceConfig, path: &str, enable: bool) -> Result<()> {
    if !enable {
        return Ok(());
    }
    let io_weight_path = path.to_string() + "/io.weight";
    trace!("resctl: verify: {:?}", &io_weight_path);
    let line = read_one_line(&io_weight_path)?;
    match scan_fmt!(&line, "default {d}", u32) {
        Ok(v) if v == sk.io_weight => {}
        v => {
            info!(
                "resctl: {:?} should be {} but is {:?}, fixing",
                &io_weight_path, sk.io_weight, &v
            );
            write_one_line(&io_weight_path, &format!("default {}", sk.io_weight))?;
        }
    }
    Ok(())
}

fn fix_cgrp_mem(path: &str, is_limit: bool, knob: MemoryKnob) -> Result<()> {
    trace!("resctl: verify: {:?}", path);
    let line = read_one_line(path)?;
    let cur = match line.as_ref() {
        "max" => Some(std::u64::MAX),
        v => v.parse::<u64>().ok(),
    };
    if let Some(mut v) = cur {
        // max can be mapped to either u64::MAX or total_memory(), limit to
        // the latter to avoid spurious mismatches.
        let target = knob.nr_bytes(is_limit).min(total_memory() as u64);
        v = v.min(total_memory() as u64);

        if target == v || (target > 0 && ((v as f64 - target as f64) / target as f64).abs() < 0.1) {
            return Ok(());
        }
    }
    let expected = mknob_to_cgrp_string(&knob, is_limit);
    info!(
        "resctl: {:?} should be {:?} but is {:?}, fixing",
        path, &expected, &line
    );
    write_one_line(path, &expected)?;

    let file = Path::new(path)
        .file_name()
        .unwrap_or(OsStr::new(""))
        .to_string_lossy();
    let cgrp = Path::new(path)
        .parent()
        .unwrap_or(Path::new(""))
        .file_name()
        .unwrap_or(OsStr::new(""))
        .to_string_lossy();

    if !cgrp.ends_with(".service") && !cgrp.ends_with(".scope") && !cgrp.ends_with(".slice") {
        return Ok(());
    }

    let mut unit = systemd::Unit::new(false, cgrp.into())?;
    let nr_bytes = knob.nr_bytes(is_limit);
    match &file[..] {
        "memory.min" => unit.resctl.mem_min = Some(nr_bytes),
        "memory.low" => unit.resctl.mem_low = Some(nr_bytes),
        "memory.high" => unit.resctl.mem_high = Some(nr_bytes),
        "memory.max" => unit.resctl.mem_max = Some(nr_bytes),
        _ => {}
    }
    unit.apply()
}

fn fix_recursive_mem_prot(parent: &str, file: &str, knob: MemoryKnob) -> Result<()> {
    for p in glob(&format!("{}/*/**/{}", parent, file))
        .unwrap()
        .filter_map(Result::ok)
    {
        if let Err(e) = fix_cgrp_mem(p.to_str().unwrap(), false, knob) {
            warn!(
                "resctl: failed to fix memory protection for {:?} ({:?})",
                p, &e
            );
        }
    }
    Ok(())
}

fn fix_slice_mem(
    sk: &SliceConfig,
    path: &str,
    enable: bool,
    verify_mem_high: bool,
    propagate_mem_prot: bool,
    recursive_mem_prot: bool,
) -> Result<()> {
    if enable {
        fix_cgrp_mem(&(path.to_string() + "/memory.min"), false, sk.mem_min)?;
        fix_cgrp_mem(&(path.to_string() + "/memory.low"), false, sk.mem_low)?;
        fix_cgrp_mem(&(path.to_string() + "/memory.max"), true, MemoryKnob::None)?;

        if verify_mem_high {
            fix_cgrp_mem(&(path.to_string() + "/memory.high"), true, sk.mem_high)?;
        }

        if propagate_mem_prot {
            if recursive_mem_prot {
                fix_recursive_mem_prot(path, "memory.min", MemoryKnob::Bytes(0))?;
                fix_recursive_mem_prot(path, "memory.low", MemoryKnob::Bytes(0))?;
            } else {
                fix_recursive_mem_prot(path, "memory.min", sk.mem_min)?;
                fix_recursive_mem_prot(path, "memory.low", sk.mem_low)?;
            }
        }
    } else {
        fix_cgrp_mem(&(path.to_string() + "/memory.min"), false, MemoryKnob::None)?;
        fix_cgrp_mem(&(path.to_string() + "/memory.low"), false, MemoryKnob::None)?;
    }
    Ok(())
}

pub fn verify_and_fix_slices(
    knobs: &SliceKnobs,
    workload_senpai: bool,
    cfg: &Config,
) -> Result<()> {
    let seq = super::instance_seq();
    let dseqs = &knobs.disable_seqs;
    let line = read_one_line("/sys/fs/cgroup/cgroup.subtree_control")?;

    if (cfg.enforce.cpu && ((dseqs.cpu < seq) != line.contains("cpu")))
        || (cfg.enforce.io && !line.contains("io"))
        || (cfg.enforce.crit_mem_prot && !line.contains("memory"))
    {
        info!("resctl: Controller enable state disagrees with overrides, fixing");
        fix_overrides(dseqs, cfg)?;
    }

    let recursive_mem_prot = cfg.memcg_recursive_prot();

    for slice in Slice::into_enum_iter() {
        let sk = knobs.slices.get(slice.name()).unwrap();

        let path = slice.cgrp();
        if !AsRef::<Path>::as_ref(path).exists() {
            continue;
        }

        if cfg.enforce.cpu {
            fix_slice_cpu(&sk, path, dseqs.cpu < seq)?;
        }
        if cfg.enforce.io {
            fix_slice_io(&sk, path, dseqs.io < seq)?;
        }

        if slice_enforce_mem(&cfg.enforce, slice) {
            let (enable_mem, verify_mem_high) = match slice {
                Slice::Work => (dseqs.mem < seq, !workload_senpai),
                _ => (true, true),
            };
            let propagate_mem_prot = slice_needs_mem_prot_propagation(slice);

            fix_slice_mem(
                &sk,
                path,
                enable_mem,
                verify_mem_high,
                propagate_mem_prot,
                recursive_mem_prot,
            )?;
        }
    }

    if cfg.enforce.io {
        check_other_io_controllers(&mut BTreeSet::new());
    }
    Ok(())
}
