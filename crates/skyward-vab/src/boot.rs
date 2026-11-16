use crate::{BootSlots, MergeStatus, Slot};
use anyhow::{bail, Context, Result};
use skyward_binder::driver::{Binder, Handle};
use skyward_binder::hidl::{self, Answer, Message};
use skyward_binder::parcel::{Parcel, Reader};
use skyward_binder::{service, status};

pub const SERVICE: &str = "android.hardware.boot.IBootControl/default";
const INTERFACE: &str = "android.hardware.boot.IBootControl";

pub const HARDWARE_SERVICE: &str = "android.hardware.boot@1.2::IBootControl";
const FIRST: &str = "android.hardware.boot@1.0::IBootControl";
const MERGE: &str = "android.hardware.boot@1.1::IBootControl";
const INSTANCE: &str = "default";

const GET_ACTIVE: u32 = 1;
const GET_MERGE_STATUS: u32 = 4;
const IS_BOOTABLE: u32 = 6;
const IS_SUCCESSFUL: u32 = 7;
const SET_ACTIVE_SLOT: u32 = 9;
const SET_SNAPSHOT_MERGE_STATUS: u32 = 11;

const CURRENT: u32 = 2;
const SET_ACTIVE: u32 = 4;
const BOOTABLE: u32 = 6;
const SUCCESSFUL: u32 = 7;
const SET_MERGE_STATUS: u32 = 9;
const MERGE_STATUS: u32 = 10;
const ACTIVE: u32 = 11;

const NO: i32 = 0;
const YES: i32 = 1;

pub enum BootControl<'a> {
    Newer(Handle<'a>),
    Older(Handle<'a>),
}

pub fn next_slot() -> Result<Slot> {
    let (binder, hardware) = (Binder::open()?, Binder::hardware()?);
    let control = BootControl::open(&binder, &hardware)?;
    control.active()
}

fn newer<T>(control: &Handle, method: u32, args: impl FnOnce(&mut Parcel), answer: impl FnOnce(&mut Reader) -> Result<T>) -> Result<T> {
    let mut call = Parcel::call(INTERFACE);
    args(&mut call);
    let reply = control.transact(method, &call)?;
    let mut read = reply.read();
    status::ok(&mut read)?;
    answer(&mut read)
}

fn older<T>(
    control: &Handle,
    version: &str,
    method: u32,
    args: impl FnOnce(&mut Message),
    answer: impl FnOnce(&mut Answer) -> Result<T>,
) -> Result<T> {
    let mut call = Message::call(version);
    args(&mut call);
    let reply = control.transact(method, &call)?;
    let mut read = Answer::new(reply.bytes());
    read.status()?;
    answer(&mut read)
}

impl<'a> BootControl<'a> {
    pub fn open(binder: &'a Binder, hardware: &'a Binder) -> Result<BootControl<'a>> {
        let not_running = |name: &str| format!("{name} is not running; the phone's boot control has to be started first");
        if service::declared(binder, SERVICE).unwrap_or(false) {
            let control = service::find(binder, SERVICE)?.with_context(|| not_running(SERVICE))?;
            return Ok(BootControl::Newer(control));
        }
        for name in [HARDWARE_SERVICE, MERGE, FIRST] {
            if let Some(control) = hidl::find(hardware, name, INSTANCE)? {
                return Ok(BootControl::Older(control));
            }
        }
        bail!("{}", not_running(HARDWARE_SERVICE))
    }

    pub fn active(&self) -> Result<Slot> {
        let number = match self {
            BootControl::Newer(c) => newer(c, GET_ACTIVE, |_| {}, |r| r.i32()),
            BootControl::Older(c) => older(c, HARDWARE_SERVICE, ACTIVE, |_| {}, |r| r.i32())
                .or_else(|_| older(c, FIRST, CURRENT, |_| {}, |r| r.i32())),
        }
        .context("ask which slot starts next")?;
        match number {
            0 => Ok(Slot::A),
            1 => Ok(Slot::B),
            other => bail!("the boot control says slot {other} starts next"),
        }
    }

    pub fn slots(&self) -> Result<BootSlots> {
        let mut slots = BootSlots { active: self.active()?, bootable: [false; 2], successful: [false; 2] };
        for slot in [Slot::A, Slot::B] {
            let at = slot.number() as usize;
            slots.bootable[at] = self.bootable(slot).with_context(|| format!("ask whether slot {} starts", slot.letter()))?;
            slots.successful[at] = self.successful(slot).with_context(|| format!("ask whether slot {} has started fully", slot.letter()))?;
        }
        Ok(slots)
    }

    fn bootable(&self, slot: Slot) -> Result<bool> {
        match self {
            BootControl::Newer(c) => newer(c, IS_BOOTABLE, |p| p.i32(slot.number() as i32), |r| r.bool()),
            BootControl::Older(c) => older(c, FIRST, BOOTABLE, |m| m.u32(slot.number()), answered),
        }
    }

    fn successful(&self, slot: Slot) -> Result<bool> {
        match self {
            BootControl::Newer(c) => newer(c, IS_SUCCESSFUL, |p| p.i32(slot.number() as i32), |r| r.bool()),
            BootControl::Older(c) => older(c, FIRST, SUCCESSFUL, |m| m.u32(slot.number()), answered),
        }
    }

    pub fn merge_status(&self) -> Result<MergeStatus> {
        let said = match self {
            BootControl::Newer(c) => newer(c, GET_MERGE_STATUS, |_| {}, |r| r.i32()),
            BootControl::Older(c) => older(c, MERGE, MERGE_STATUS, |_| {}, |r| r.i32()),
        }
        .context("ask where an update stands")?;
        Ok(match said {
            0 => MergeStatus::None,
            1 => MergeStatus::Unknown,
            2 => MergeStatus::Snapshotted,
            3 => MergeStatus::Merging,
            4 => MergeStatus::Cancelled,
            other => bail!("the boot control says an update stands at {other}"),
        })
    }

    pub fn set_merge_status(&self, status: MergeStatus) -> Result<()> {
        let what = || format!("tell the bootloader an update stands at {status:?}");
        let done = match self {
            BootControl::Newer(c) => newer(c, SET_SNAPSHOT_MERGE_STATUS, |p| p.i32(status as i32), |_| Ok(true)),
            BootControl::Older(c) => older(c, MERGE, SET_MERGE_STATUS, |m| m.u32(status as u32), |r| r.bool()),
        }
        .with_context(what)?;
        if !done {
            bail!("the boot control would not {}", what());
        }
        Ok(())
    }

    pub fn set_active(&self, slot: Slot) -> Result<()> {
        let what = || format!("have slot {} start next", slot.letter());
        match self {
            BootControl::Newer(c) => newer(c, SET_ACTIVE_SLOT, |p| p.i32(slot.number() as i32), |_| Ok(())).with_context(what),
            BootControl::Older(c) => {
                let why = older(c, FIRST, SET_ACTIVE, |m| m.u32(slot.number()), |r| {
                    let done = r.pointed()?;
                    if done.first().is_some_and(|&b| b != 0) {
                        return Ok(None);
                    }
                    let why = r.pointed()?;
                    Ok(Some(String::from_utf8_lossy(why.split(|&b| b == 0).next().unwrap_or_default()).into_owned()))
                })
                .with_context(what)?;
                match why {
                    None => Ok(()),
                    Some(why) => bail!("the boot control would not {}: {why}", what()),
                }
            }
        }
    }
}

fn answered(read: &mut Answer) -> Result<bool> {
    match read.i32()? {
        YES => Ok(true),
        NO => Ok(false),
        other => bail!("the boot control answered {other}"),
    }
}
