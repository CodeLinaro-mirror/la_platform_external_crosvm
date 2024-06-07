// Copyright 2023 The ChromiumOS Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use base::sched_attr;
use base::sched_setattr;
use base::warn;
use base::Error;
use std::os::unix::net::UnixStream;
use std::sync::Arc;

use sync::Mutex;

use crate::pci::CrosvmDeviceId;
use crate::BusAccessInfo;
use crate::BusDevice;
use crate::DeviceId;
use crate::Suspendable;

const CPUFREQ_GOV_SCALE_FACTOR_DEFAULT: u32 = 100;
const CPUFREQ_GOV_SCALE_FACTOR_SCHEDUTIL: u32 = 80;

const SCHED_FLAG_RESET_ON_FORK: u64 = 0x1;
const SCHED_FLAG_KEEP_POLICY: u64 = 0x08;
const SCHED_FLAG_KEEP_PARAMS: u64 = 0x10;
const SCHED_FLAG_UTIL_CLAMP_MIN: u64 = 0x20;

const SCHED_FLAG_KEEP_ALL: u64 = SCHED_FLAG_KEEP_POLICY | SCHED_FLAG_KEEP_PARAMS;

pub struct VirtCpufreq {
    cpu_fmax: u32,
    cpu_capacity: u32,
    pcpu: u32,
    util_factor: u32,
}

fn get_cpu_info(cpu_id: u32, property: &str) -> Result<u32, Error> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/{property}");
    std::fs::read_to_string(path)?
        .trim()
        .parse()
        .map_err(|_| Error::new(libc::EINVAL))
}

fn get_cpu_info_str(cpu_id: u32, property: &str) -> Result<String, Error> {
    let path = format!("/sys/devices/system/cpu/cpu{cpu_id}/{property}");
    std::fs::read_to_string(path).map_err(|_| Error::new(libc::EINVAL))
}

fn get_cpu_capacity(cpu_id: u32) -> Result<u32, Error> {
    get_cpu_info(cpu_id, "cpu_capacity")
}

fn get_cpu_maxfreq_khz(cpu_id: u32) -> Result<u32, Error> {
    get_cpu_info(cpu_id, "cpufreq/cpuinfo_max_freq")
}

fn get_cpu_curfreq_khz(cpu_id: u32) -> Result<u32, Error> {
    get_cpu_info(cpu_id, "cpufreq/scaling_cur_freq")
}

fn handle_read_err(err: Error) -> String {
    warn!("Unable to get cpufreq governor, using 100% default util factor. Err: {:?}", err);
    "unknown_governor".to_string()
}

fn get_cpu_util_factor(cpu_id: u32) -> Result<u32, Error> {
    let gov = get_cpu_info_str(cpu_id, "cpufreq/scaling_governor").unwrap_or_else(handle_read_err);
    match gov.trim() {
        "schedutil" => Ok(CPUFREQ_GOV_SCALE_FACTOR_SCHEDUTIL),
        _ => Ok(CPUFREQ_GOV_SCALE_FACTOR_DEFAULT),
    }
}

impl VirtCpufreq {
    pub fn new(pcpu: u32, _socket: Option<Arc<Mutex<UnixStream>>>) -> Self {
        let cpu_capacity = get_cpu_capacity(pcpu).expect("Error reading capacity");
        let cpu_fmax = get_cpu_maxfreq_khz(pcpu).expect("Error reading max freq");
        let util_factor = get_cpu_util_factor(pcpu).expect("Error getting util factor");

        VirtCpufreq {
            cpu_fmax,
            cpu_capacity,
            pcpu,
            util_factor,
        }
    }
}

impl BusDevice for VirtCpufreq {
    fn device_id(&self) -> DeviceId {
        CrosvmDeviceId::VirtCpufreq.into()
    }

    fn debug_label(&self) -> String {
        "VirtCpufreq Device".to_owned()
    }

    fn read(&mut self, _info: BusAccessInfo, data: &mut [u8]) {
        if data.len() != std::mem::size_of::<u32>() {
            warn!(
                "{}: unsupported read length {}, only support 4bytes read",
                self.debug_label(),
                data.len()
            );
            return;
        }
        // TODO(davidai): Evaluate opening file and re-reading the same fd.
        let freq = match get_cpu_curfreq_khz(self.pcpu) {
            Ok(freq) => freq,
            Err(e) => panic!("{}: Error reading freq: {}", self.debug_label(), e),
        };

        let freq_arr = freq.to_ne_bytes();
        data.copy_from_slice(&freq_arr);
    }

    fn write(&mut self, _info: BusAccessInfo, data: &[u8]) {
        let freq: u32 = match data.try_into().map(u32::from_ne_bytes) {
            Ok(v) => v,
            Err(e) => {
                warn!(
                    "{}: unsupported write length {}, only support 4bytes write",
                    self.debug_label(),
                    e
                );
                return;
            }
        };

        // Util margin depends on the cpufreq governor on the host
        let cpu_cap_scaled = self.cpu_capacity * self.util_factor / CPUFREQ_GOV_SCALE_FACTOR_DEFAULT;
        let util = cpu_cap_scaled * freq / self.cpu_fmax;

        let mut sched_attr = sched_attr::default();
        sched_attr.sched_flags =
            SCHED_FLAG_KEEP_ALL | SCHED_FLAG_UTIL_CLAMP_MIN | SCHED_FLAG_RESET_ON_FORK;
        sched_attr.sched_util_min = util;

        if let Err(e) = sched_setattr(0, &mut sched_attr, 0) {
            panic!("{}: Error setting util value: {}", self.debug_label(), e);
        }
    }
}

impl Suspendable for VirtCpufreq {}
