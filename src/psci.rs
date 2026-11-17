//! PSCI 1.1 firmware calls (HVC / SMC with the function ID in x0), shared by the AArch64
//! interpreter and the hardware accelerators. CPU power state lives in `Bus::ctl`.

use crate::bus::Bus;
use crate::machine::Stop;

/// Outcome of a PSCI call.
pub enum Psci {
    /// Return this value in x0.
    Ret(u64),
    /// CPU_OFF: the calling CPU is now parked (x0 = 0 if it is ever looked at again).
    Off,
    /// SYSTEM_OFF / SYSTEM_RESET.
    Stop(Stop),
}

const NOT_SUPPORTED: u64 = -1i64 as u64;
const INVALID_PARAMS: u64 = -2i64 as u64;
const ALREADY_ON: u64 = -4i64 as u64;
const ON_PENDING: u64 = -5i64 as u64;

/// Handle a call from CPU `id` with arguments `x[0..4]`.
pub fn call(bus: &mut Bus, id: usize, x: [u64; 4]) -> Psci {
    let [fid, a1, a2, a3] = x;
    Psci::Ret(match fid as u32 {
        0x8400_0008 => return Psci::Stop(Stop::Halt(0)), // SYSTEM_OFF
        0x8400_0009 => return Psci::Stop(Stop::Reset),   // SYSTEM_RESET
        0x8400_0000 => 0x0001_0001,                      // PSCI_VERSION 1.1
        0x8400_0002 => {
            // CPU_OFF: this CPU powers down and never returns
            bus.ctl[id].parked = true;
            return Psci::Off;
        }
        0x8400_0003 | 0xc400_0003 => {
            // CPU_ON(target_cpu, entry_point, context_id)
            let t = (a1 & 0xff) as usize;
            if a1 & !0xff != 0 && a1 & !0xff != 0x8000_0000 || t >= bus.ncpus {
                INVALID_PARAMS
            } else if bus.ctl[t].start.is_some() {
                ON_PENDING
            } else if !bus.ctl[t].parked {
                ALREADY_ON
            } else {
                bus.ctl[t].start = Some((a2, a3));
                0
            }
        }
        0x8400_0004 | 0xc400_0004 => {
            // AFFINITY_INFO: 0 = ON, 1 = OFF, 2 = ON_PENDING
            let t = (a1 & 0xff) as usize;
            if t >= bus.ncpus {
                INVALID_PARAMS
            } else if bus.ctl[t].start.is_some() {
                2
            } else {
                bus.ctl[t].parked as u64
            }
        }
        0x8400_0006 => 2, // MIGRATE_INFO_TYPE: trusted OS not present / not migratable
        0x8400_000a => match a1 as u32 {
            // PSCI_FEATURES
            0x8400_0000 | 0x8400_0002 | 0x8400_0003 | 0xc400_0003 | 0x8400_0004 | 0xc400_0004 | 0x8400_0006
            | 0x8400_0008 | 0x8400_0009 | 0x8400_000a => 0,
            _ => NOT_SUPPORTED,
        },
        _ => NOT_SUPPORTED,
    })
}
