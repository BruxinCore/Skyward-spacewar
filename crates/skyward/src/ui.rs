use crate::backup;
use crate::decrypt::{self, Decryptor};
use crate::filework;
use crate::haptics::Haptics;
use crate::relabel;
use crate::input::{monotonic_ms, Event, Inputs};
use crate::merge;
use crate::partwork;
use crate::pty;
use crate::readings::{Reading, Readings};
use crate::secure;
use crate::services;
use crate::settings;
use crate::systems;
use crate::updates;
use crate::wipe::{self, Eraser, Step};
use crate::wlan;
use anyhow::{anyhow, ensure, Context, Result};
use skyward_block::fsck::Fixed;
use skyward_block::{fstab, mount, probe};
use skyward_config::{DeviceConfig, Service};
use skyward_crypto::data::{Closed, Opened};
use skyward_install::asked::{self, Needs};
use skyward_install::{Detach, Installer, Options};
use skyward_sys::bcb::{self, Work, REFORMATS};
use skyward_sys::control::{Control, Request};
use skyward_sys::Power;
use skyward_files::Ownership;
use skyward_block::usb::{self, Drive};
use skyward_net::download::{self, Failure};
use skyward_net::http::Servers;
use skyward_net::release::{Installed, Release, Unanswered};
use skyward_net::wlan::{Power as WlanPower, Stage as Joining, Wlan};
use skyward_sys::sleep::{Sleeper, Slept};
use skyward_sys::uevent;
use skyward_systems::Who;
use skyward_ui::app::{
    App, Asked, BackupAt, BackupEnd, BackupEntry, BackupRefusal, ClashChoice, Data, DownloadEnd, Effect, EraseStop, FileKind, FileOp,
    FileRow, FileTask, FsAction, FsRow, Have, Held, ItemEnd, ItemState, JobEnd, Keys, MountFailure, Part, PartKind, PartStop, Place,
    RenameRefused, Screen, Shot, SkywardEnd, SkywardStage, Target, UsbDrive, UsbLink, Wipe, DATA_OPENING, DATA_PARTITION, DOWNLOADS,
    INSTALL_UNFINISHED, USB_DRIVE,
};
use skyward_ui::settings::Keeper;
use skyward_ui::surface::kms::KmsSurface;
use skyward_ui::{Canvas, Fonts, Lang, Surface};
use skyward_vab::{Merge, Standing};
use std::collections::{BTreeSet, VecDeque};
use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const USB_TIMEOUT: Duration = Duration::from_secs(5);

const LEAVE_AFTER_MS: u64 = 1500;

const TOUCH_WAIT: Duration = Duration::from_secs(5);

const REST: Duration = Duration::from_secs(5);

const TMP_LOGS: &str = "/skyward/run/logs";
const STORAGE_LOGS: &str = "Skyward/logs";

const SCREENSHOTS: &str = "DCIM/Screenshots";
const HELD_SCREENSHOTS: &str = "/skyward/run/screenshots";

struct Backlight {
    path: PathBuf,
    top: u32,
    level: u32,
}

impl Backlight {
    fn open() -> Result<Self> {
        let mut dirs: Vec<PathBuf> = fs::read_dir("/sys/class/backlight")
            .context("read /sys/class/backlight")?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        dirs.sort();
        let dir = dirs
            .iter()
            .find(|d| d.file_name().is_some_and(|n| n == "panel0-backlight"))
            .or(dirs.first())
            .ok_or_else(|| anyhow!("no backlight device"))?
            .clone();
        let read = |name: &str| -> Result<u32> {
            fs::read_to_string(dir.join(name))
                .with_context(|| format!("read {}/{name}", dir.display()))?
                .trim()
                .parse()
                .with_context(|| format!("parse {name}"))
        };
        let max = read("max_brightness")?;
        let top = normal_max(max).unwrap_or(max);
        let level = read("brightness")?;
        Ok(Backlight { path: dir.join("brightness"), top, level })
    }

    fn show(&mut self, level: Option<f32>) -> Result<()> {
        let raw = level.map_or(0, |v| ((v.clamp(0.0, 1.0) * self.top as f32).round() as u32).max(1));
        if raw != self.level {
            fs::write(&self.path, raw.to_string()).with_context(|| format!("write {}", self.path.display()))?;
            eprintln!("backlight {raw}");
            self.level = raw;
        }
        Ok(())
    }
}

const NORMAL_MAX: [(&str, &str); 3] = [
    ("mi,normal-max-brightness-clone", "mi,max-brightness-clone"),
    ("mi,max-brightness-normal", "qcom,mdss-dsi-bl-max-level"),
    ("mi,mdss-dsi-fac-bl-max-level", "qcom,mdss-dsi-bl-max-level"),
];

fn normal_max(max: u32) -> Result<u32> {
    let param = "/sys/module/msm_drm/parameters/dsi_display0";
    let boot = fs::read_to_string(param).with_context(|| format!("read {param}"))?;
    let (name, _) = boot.trim().split_once(':').with_context(|| format!("{param} names no panel: {boot:?}"))?;
    let soc = Path::new("/proc/device-tree/soc");
    let node = fs::read_dir(soc)
        .with_context(|| format!("read {}", soc.display()))?
        .map(|e| e.map(|e| e.path().join(name)))
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .find(|p| p.is_dir())
        .ok_or_else(|| anyhow!("panel {name} is not in the device tree"))?;
    let cell = |prop: &str| -> Result<u32> {
        let path = node.join(prop);
        let bytes = fs::read(&path).with_context(|| format!("read {}", path.display()))?;
        let cell: [u8; 4] = bytes.as_slice().try_into().map_err(|_| anyhow!("{} is not one cell", path.display()))?;
        Ok(u32::from_be_bytes(cell))
    };
    let (normal, full) = NORMAL_MAX
        .iter()
        .find(|(normal, _)| node.join(normal).exists())
        .with_context(|| format!("panel {name} gives its normal maximum as none of {NORMAL_MAX:?}"))?;
    let (normal, full) = (cell(normal)?, cell(full)?);
    ensure!(normal > 0 && normal <= full, "panel {name}: normal maximum {normal} out of {full}");
    Ok((f64::from(max) * f64::from(normal) / f64::from(full)).round() as u32)
}

fn power(target: Target) -> Result<()> {
    skyward_sys::power(match target {
        Target::System => Power::System,
        Target::Recovery => Power::Recovery,
        Target::Fastboot => Power::Fastboot,
        Target::Bootloader => Power::Bootloader,
        Target::Edl => Power::Edl,
        Target::PowerOff => Power::Off,
    })
}

struct Transfer {
    service: skyward_mtp::Service,
    offered: bool,
    zone: usize,
}

impl Transfer {
    fn start(cfg: &DeviceConfig, info: &skyward_info::DeviceInfo, zone: usize) -> Result<Transfer> {
        let device = skyward_mtp::Device {
            manufacturer: cfg.device.brand.clone(),
            model: info.model.clone(),
            version: format!("Skyward {}", skyward_info::VERSION),
            serial: skyward_sys::prop("ro.serialno").unwrap_or_default(),
            name: info.model.clone(),
            battery: Box::new(|| skyward_info::battery_now().ok().flatten().map(|b| b.level)),
        };
        let service = skyward_mtp::Service::start(device, skyward_ui::fmt::rules(zone).clone())?;
        skyward_sys::usb::set_mtp(true);
        Ok(Transfer { service, offered: false, zone })
    }

    fn offer(&mut self, offer: bool, lang: Lang) -> Result<()> {
        if offer != self.offered {
            let description = skyward_ui::fmt::storage_name(lang).into();
            let storage = offer.then(|| skyward_mtp::Storage { top: PathBuf::from(skyward_crypto::data::SDCARD), description });
            self.service.offer(storage)?;
            self.offered = offer;
        }
        Ok(())
    }

    fn stop(self, rests: bool) -> Result<()> {
        skyward_sys::usb::set_mtp(false);
        let switched = if rests { skyward_sys::usb::switch("adb", USB_TIMEOUT) } else { Ok(()) };
        let stopped = self.service.stop();
        switched.and(stopped)
    }
}

fn withhold(transfer: &mut Option<Transfer>, app: &mut App, now: u64) -> Result<bool> {
    let Some(t) = transfer else { return Ok(true) };
    match t.offer(false, app.state.lang) {
        Ok(()) => Ok(true),
        Err(e) => {
            skyward_sys::kmsg(&format!("file transfer kept internal storage: {e:#}"))?;
            app.report(&skyward_ui::fmt::transfer_kept(app.state.lang, &format!("{e:#}")), now);
            Ok(false)
        }
    }
}

fn fastboot_status() -> Result<(bool, UsbLink)> {
    let running = skyward_sys::prop("init.svc.fastbootd").as_deref() == Some("running");
    Ok((running, usb_link()?))
}

fn plug(app: &mut App, was: &Option<skyward_info::Battery>, is: &Option<skyward_info::Battery>, now: u64) -> Result<()> {
    let plugged = |b: &Option<skyward_info::Battery>| b.as_ref().is_some_and(|b| b.plugged);
    if plugged(was) != plugged(is) {
        skyward_sys::kmsg(if plugged(is) { "a charger or a computer powers the phone" } else { "the phone runs on its battery" })?;
        app.plugged(now, plugged(is));
    }
    Ok(())
}

#[derive(Default)]
struct Untaken {
    battery: skyward_sys::ToldOnce,
    details: skyward_sys::ToldOnce,
}

fn untaken(told: &mut skyward_sys::ToldOnce, what: &str, why: Option<&anyhow::Error>) -> Result<()> {
    let why = why.map(|e| format!("{e:#}"));
    if told.untold(why.as_deref()) {
        skyward_sys::kmsg(&format!("{what} could not be read, and the pages keep what they have: {}", why.unwrap_or_default()))?;
    }
    Ok(())
}

fn read_details(cfg: &DeviceConfig, info: &mut skyward_info::DeviceInfo, told: &mut skyward_sys::ToldOnce) -> Result<()> {
    let read = skyward_info::collect(cfg);
    untaken(told, "the phone's details", read.as_ref().err())?;
    if let Ok(read) = read {
        *info = read;
    }
    Ok(())
}

fn usb_link() -> Result<UsbLink> {
    let controller = skyward_sys::prop("sys.usb.controller").context("init names no USB controller")?;
    let path = Path::new("/sys/class/udc").join(controller).join("state");
    let state = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    Ok(match state.trim() {
        "configured" => UsbLink::Connected,
        "not attached" => UsbLink::Unplugged,
        _ => UsbLink::Connecting,
    })
}

fn asked(request: &bcb::Request) -> Asked {
    Asked {
        reason: request.reason.clone(),
        shutdown_after: request.shutdown_after,
        unhandled: match &request.work {
            Work::Unsupported(what) => Some(what.clone()),
            _ => None,
        },
    }
}

fn list(app: &mut App, folder: String) -> Result<()> {
    let files = read_folder(Path::new(&folder)).map_err(|e| format!("{e:#}"));
    app.set_files(folder, files, unix_now()?);
    Ok(())
}

fn list_place(app: &mut App, place: Place, folder: String) -> Result<()> {
    let files = read_folder(&in_place(place, &folder)).map_err(|e| format!("{e:#}"));
    app.set_place(place, folder, files, unix_now()?);
    Ok(())
}

fn in_place(place: Place, path: &str) -> PathBuf {
    let top = match place {
        Place::Storage => Path::new(skyward_crypto::data::SDCARD),
        Place::UsbDrive => Path::new(USB_DRIVE),
    };
    if path.is_empty() {
        top.to_path_buf()
    } else {
        top.join(path)
    }
}

fn export_logs(recovery: bool, kernel: bool, into: &Path, zone: usize) -> Result<PathBuf> {
    let there = into.ancestors().find(|p| p.exists()).context("the root is always there")?;
    let ownership = ownership(there)?;
    let folder = into.join(skyward_ui::fmt::stamp(unix_now()?, zone));
    skyward_files::make_folders(&folder, ownership)?;
    let save = |name: &str, contents: &[u8]| -> Result<()> {
        let path = folder.join(name);
        let mut file = skyward_files::make_file(&path, ownership)?;
        file.write_all(contents).with_context(|| format!("write {}", path.display()))?;
        file.sync_all().with_context(|| format!("write {}", path.display()))
    };
    if recovery {
        save("recovery.log", &fs::read(skyward_sys::LOG).with_context(|| format!("read {}", skyward_sys::LOG))?)?;
        for (name, path) in [("update_engine.log", skyward_install::ENGINE_LOG), ("install-script.log", skyward_install::SCRIPT_LOG)] {
            match fs::read(path) {
                Ok(log) => save(name, &log)?,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("read {path}")),
            }
        }
    }
    if kernel {
        save("kernel.log", skyward_sys::kernel_log()?.as_bytes())?;
    }
    Ok(folder)
}

fn save_screenshot(frame: &Canvas, storage: bool, zone: usize) -> Result<Shot> {
    let (folder, shot) = match storage {
        true => (Path::new(skyward_crypto::data::SDCARD).join(SCREENSHOTS), Shot::Album),
        false => (PathBuf::from(HELD_SCREENSHOTS), Shot::Held),
    };
    let there = folder.ancestors().find(|p| p.exists()).context("the root is always there")?;
    let ownership = ownership(there)?;
    skyward_files::make_folders(&folder, ownership)?;
    let path = folder.join(skyward_ui::fmt::screenshot_name(unix_ms_now()?, zone));
    let file = skyward_files::make_file(&path, ownership)?;
    let mut out = std::io::BufWriter::new(&file);
    frame.write_png(&mut out).with_context(|| format!("write {}", path.display()))?;
    out.flush().with_context(|| format!("write {}", path.display()))?;
    drop(out);
    file.sync_all().with_context(|| format!("write {}", path.display()))?;
    skyward_sys::kmsg(&format!("screenshot saved as {}", path.display()))?;
    Ok(shot)
}

fn keep_held_screenshots() -> Result<()> {
    let held = Path::new(HELD_SCREENSHOTS);
    let shots = match fs::read_dir(held) {
        Ok(entries) => entries.map(|e| e.map(|e| e.path())).collect::<std::io::Result<Vec<_>>>().with_context(|| format!("read {}", held.display()))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("read {}", held.display())),
    };
    if shots.is_empty() {
        return Ok(());
    }
    let album = Path::new(skyward_crypto::data::SDCARD).join(SCREENSHOTS);
    let there = album.ancestors().find(|p| p.exists()).context("the root is always there")?;
    let ownership = ownership(there)?;
    skyward_files::make_folders(&album, ownership)?;
    let stop = std::sync::atomic::AtomicBool::new(false);
    skyward_files::move_into(&shots, &album, skyward_files::Clash::KeepBoth, ownership, &stop, &mut |_| {})?;
    skyward_sys::kmsg(&format!("{} screenshots held in {} moved into {}", shots.len(), held.display(), album.display()))
}

fn backups_in(place: Place) -> PathBuf {
    in_place(place, skyward_backup::FOLDER)
}

fn backups_at(place: Place) -> Result<Vec<BackupEntry>> {
    let folder = backups_in(place);
    let entries = match fs::read_dir(&folder) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("read {}", folder.display())),
    };
    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.with_context(|| format!("read {}", folder.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.file_type().with_context(|| format!("look at {}", entry.path().display()))?.is_dir() {
            continue;
        }
        let held = skyward_backup::Manifest::read(&entry.path()).map(|m| held(&m)).map_err(|e| format!("{e:#}"));
        found.push(BackupEntry { place, name, held });
    }
    Ok(found)
}

fn held(m: &skyward_backup::Manifest) -> Held {
    Held {
        made: m.made,
        size: m.size(),
        parts: m.parts(),
        device: m.device.clone(),
        system: m.system.clone(),
        slot: skyward_vab::Slot::from_suffix(&m.slot),
    }
}

fn find_backups(app: &mut App, storage: bool, usb: bool, now: u64) {
    let mut found = Vec::new();
    for (place, look) in [(Place::Storage, storage), (Place::UsbDrive, usb)] {
        if look {
            match backups_at(place) {
                Ok(mut there) => found.append(&mut there),
                Err(e) => app.report(&format!("{e:#}"), now),
            }
        }
    }
    app.set_backups(found);
}

fn delete_backup(place: Place, folder: &Path) -> Result<()> {
    let drive = Path::new(USB_DRIVE);
    if place == Place::UsbDrive {
        skyward_block::mount::remount_writable(drive, true)?;
    }
    let deleted = fs::remove_dir_all(folder).with_context(|| format!("delete {}", folder.display()));
    if place == Place::UsbDrive {
        let read_only = skyward_block::mount::remount_writable(drive, false);
        deleted?;
        return read_only;
    }
    deleted
}

fn backup_end(end: backup::End) -> BackupEnd {
    match end {
        backup::End::Saved(folder) => {
            BackupEnd::Saved(folder.file_name().expect("a backup's folder has a name").to_string_lossy().into_owned())
        }
        backup::End::Restored { unmade } => BackupEnd::Restored { unmade: unmade.iter().map(|p| p.display().to_string()).collect() },
        backup::End::Stopped => BackupEnd::Stopped,
        backup::End::Refused(refusal) => BackupEnd::Refused(match refusal {
            backup::Refusal::Room { needed, free } => BackupRefusal::Room { needed, free },
            backup::Refusal::Update(state) => BackupRefusal::Update(state),
            backup::Refusal::OtherPhone(device) => BackupRefusal::OtherPhone(device),
            backup::Refusal::Resized(name) => BackupRefusal::Resized(name),
            backup::Refusal::DataReset => BackupRefusal::DataReset,
        }),
        backup::End::Failed(why) => BackupEnd::Failed(why),
    }
}

fn unix_now() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH).context("the clock is before 1970")?.as_secs() as i64)
}

fn unix_ms_now() -> Result<i64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH).context("the clock is before 1970")?.as_millis() as i64)
}

struct Plugged {
    drive: Drive,
    shown: Result<(), String>,
}

fn look_for_drive(plugged: &mut Option<Plugged>, seen: &mut Option<String>) -> Result<Option<Option<UsbDrive>>> {
    let found = usb::drives()?.into_iter().find(Drive::ready);
    if let Some(p) = plugged {
        if found.is_some_and(|d| d.disk == p.drive.disk) {
            return Ok(None);
        }
        if p.shown.is_ok() {
            skyward_block::mount::umount_detach(Path::new(USB_DRIVE))?;
        }
        skyward_sys::kmsg(&format!("USB drive {} is gone", p.drive.disk))?;
        *plugged = None;
        *seen = None;
        return Ok(Some(None));
    }
    let Some(drive) = found else {
        *seen = None;
        return Ok(None);
    };
    if seen.as_deref() != Some(drive.disk.as_str()) {
        *seen = Some(drive.disk.clone());
        return Ok(None);
    }
    let shown = match usb::mount(&drive, Path::new(USB_DRIVE)) {
        Ok((part, filesystem)) => {
            skyward_sys::kmsg(&format!("USB drive {part} shown at {USB_DRIVE} as {filesystem}"))?;
            Ok(())
        }
        Err(e) => {
            skyward_sys::kmsg(&format!("USB drive {} not shown: {e:#}", drive.disk))?;
            Err(format!("{e:#}"))
        }
    };
    let told = match &shown {
        Ok(()) => UsbDrive::Shown,
        Err(why) => UsbDrive::Unreadable(why.clone()),
    };
    *plugged = Some(Plugged { drive, shown });
    *seen = None;
    Ok(Some(Some(told)))
}

fn clash_rule(choice: ClashChoice) -> skyward_files::Clash {
    match choice {
        ClashChoice::Replace => skyward_files::Clash::Replace,
        ClashChoice::Skip => skyward_files::Clash::Skip,
        ClashChoice::KeepBoth => skyward_files::Clash::KeepBoth,
    }
}

fn ownership(into: &Path) -> Result<Ownership> {
    use skyward_crypto::data;
    let at = Path::new(data::MOUNT);
    if !skyward_block::mount::is_mount_point(at)? {
        return Ok(Ownership::Kept);
    }
    let here = fs::metadata(into).with_context(|| format!("look at {}", into.display()))?;
    if here.dev() != fs::metadata(at).with_context(|| format!("look at {}", at.display()))?.dev() {
        return Ok(Ownership::Kept);
    }
    let real = fs::canonicalize(into).with_context(|| format!("find {}", into.display()))?;
    let shared = [data::SHARED_STORAGE, data::SDCARD].iter().any(|s| real.starts_with(s));
    Ok(if shared { Ownership::Shared } else { Ownership::Folder })
}

fn part(step: Step) -> Part {
    match step {
        Step::Data => Part::Data,
        Step::Cache => Part::Cache,
        Step::Metadata => Part::Metadata,
        Step::AppData => Part::AppData,
        Step::Media => Part::Media,
        Step::AppCaches => Part::AppCaches,
        Step::SystemCaches => Part::SystemCaches,
        Step::Art => Part::Art,
        Step::Logs => Part::Logs,
        Step::Updates => Part::Updates,
    }
}

fn step(choice: Wipe) -> Step {
    match choice {
        Wipe::AppData => Step::AppData,
        Wipe::Media => Step::Media,
        Wipe::AppCaches => Step::AppCaches,
        Wipe::SystemCaches => Step::SystemCaches,
        Wipe::Art => Step::Art,
        Wipe::Cache => Step::Cache,
        Wipe::Logs => Step::Logs,
        Wipe::Updates => Step::Updates,
        Wipe::All => unreachable!("everything is a factory reset, not a part of one"),
    }
}

fn erase_stop(stop: wipe::Stop) -> EraseStop {
    match stop {
        wipe::Stop::Refused(wipe::Refusal::Update(hold)) => EraseStop::Update(hold),
        wipe::Stop::Refused(wipe::Refusal::NotRebuilt) => EraseStop::NotRebuilt,
        wipe::Stop::Refused(wipe::Refusal::NoCache) => EraseStop::NoCache,
        wipe::Stop::Closed(why) => EraseStop::Closed(why),
        wipe::Stop::Locked(users) => EraseStop::Locked(users),
        wipe::Stop::Failed(step, e) => EraseStop::Failed(part(step), format!("{e:#}")),
    }
}

fn factory_reset(app: &mut App, keep_memtag_mode: bool, asked: bool, reformat: Option<String>) -> Result<Eraser> {
    match wipe::factory_reset(reformat) {
        Ok(plan) => {
            app.erase_started(true, asked, plan.steps().into_iter().map(part).collect());
            Eraser::start(move |tell| plan.carry_out(keep_memtag_mode, tell))
        }
        Err(stop) => {
            app.erase_started(true, asked, Vec::new());
            Eraser::stopped(stop)
        }
    }
}

fn rescue_wipe(app: &mut App) -> Result<Eraser> {
    app.rescue_wiping();
    match wipe::factory_reset(None) {
        Ok(plan) => Eraser::start(move |tell| plan.carry_out(false, tell)),
        Err(stop) => Eraser::stopped(stop),
    }
}

fn parts_wipe(app: &mut App, steps: Vec<Step>, logs: &[String], asked: bool, reformat: Option<String>) -> Result<Eraser> {
    match wipe::parts(steps, logs, reformat) {
        Ok(parts) => {
            app.erase_started(false, asked, parts.steps().into_iter().map(part).collect());
            Eraser::start(move |tell| parts.carry_out(tell))
        }
        Err(stop) => {
            app.erase_started(false, asked, Vec::new());
            Eraser::stopped(stop)
        }
    }
}

const PANICS: [&str; 2] = ["kernel_panic", "Panic"];

pub fn panicked() -> bool {
    skyward_sys::prop("ro.boot.bootreason").is_some_and(|reason| PANICS.iter().any(|p| p.eq_ignore_ascii_case(&reason)))
}

struct Pending {
    package: asked::Asked,
    needs: Needs,
    retry: bool,
}

fn install_asked(programs: &[Service], app: &App, pending: Pending) -> Result<Installer> {
    if pending.needs == Needs::Cache {
        let cache = wipe::cache()?.context("the phone has no cache partition")?;
        let at = Path::new(&cache.mount_point);
        if !mount::is_mount_point(at)? {
            fs::create_dir_all(at).with_context(|| format!("make {}", at.display()))?;
            mount::mount_ro(Path::new(&fstab::device(&cache)?), at)?;
        }
    }
    Installer::asked(pending.package, pending.retry, detach(programs.to_vec()), install_options(app))
}

fn rescue_target(target: &str) -> (Target, bool) {
    match target {
        "bootloader" => (Target::Bootloader, false),
        "fastboot" => (Target::Fastboot, false),
        "recovery" => (Target::Recovery, false),
        "rescue" => (Target::Recovery, true),
        _ => (Target::System, false),
    }
}

fn item_end(item: partwork::Item) -> ItemEnd {
    match item {
        partwork::Item::Erased => ItemEnd::Erased,
        partwork::Item::Unshared => ItemEnd::Unshared,
        partwork::Item::NotShared(filesystem) => ItemEnd::NotShared(filesystem),
        partwork::Item::Fixed(Fixed::Clean) => ItemEnd::Clean,
        partwork::Item::Fixed(Fixed::Fixed) => ItemEnd::Fixed,
        partwork::Item::Made => ItemEnd::Made,
        partwork::Item::Fixed(Fixed::Failed) => ItemEnd::Failed(None),
        partwork::Item::Failed(why) => ItemEnd::Failed(Some(why)),
    }
}

fn part_stop(stop: partwork::Stop) -> PartStop {
    match stop {
        partwork::Stop::Refused(refusal) => PartStop::Refused(refusal),
        partwork::Stop::Unmounted(base) => PartStop::Unmounted(base),
        partwork::Stop::Failed(why) => PartStop::Failed(why),
    }
}

fn read_filesystems() -> Result<Vec<FsRow>> {
    let mut rows = Vec::new();
    for (name, point) in [(DATA_PARTITION, "/data"), ("metadata", "/metadata"), ("cache", "/cache")] {
        let entries = fstab::read(point)?;
        let Some(entry) = entries.first() else { continue };
        let device = fstab::device(entry)?;
        let size = skyward_block::size_bytes(&device)?;
        let mut listed: Vec<String> = Vec::new();
        for e in &entries {
            if REFORMATS.contains(&e.filesystem.as_str()) && !listed.contains(&e.filesystem) {
                listed.push(e.filesystem.clone());
            }
        }
        let filesystem = match name {
            DATA_PARTITION => mount::visible(Path::new(point))?.map(|v| v.filesystem),
            _ => probe::filesystem(Path::new(&device))?,
        };
        let others = match (name, &filesystem) {
            ("metadata", _) => Vec::new(),
            (_, Some(held)) => listed.into_iter().filter(|fs| fs != held).collect(),
            (_, None) if listed.len() > 1 => listed,
            (_, None) => Vec::new(),
        };
        rows.push(FsRow { name: name.to_string(), mount_point: point.to_string(), filesystem, size, others });
    }
    Ok(rows)
}

fn fs_part(name: &str) -> Result<partwork::Part> {
    if name == DATA_PARTITION {
        return Ok(partwork::Part::Data);
    }
    let point = format!("/{name}");
    let entry = fstab::read(&point)?.into_iter().next().with_context(|| format!("the phone's table has no {point}"))?;
    Ok(partwork::Part::Entry(entry))
}

enum Incoming {
    Arrived,
    Input(Event),
    Asked(Request),
    Drive(Option<UsbDrive>),
}

fn answer(request: Request, fastboot: bool) -> Vec<Effect> {
    match (request, fastboot) {
        (Request::Fastboot, false) => vec![Effect::Reboot(Target::Fastboot)],
        (Request::Recovery, true) => vec![Effect::EnterRecovery],
        _ => Vec::new(),
    }
}

fn clock(zone: usize) -> Result<String> {
    Ok(skyward_ui::fmt::clock(unix_now()?, zone))
}

fn log_mounted(mounted: &Opened) -> Result<()> {
    if let Some(checked) = &mounted.checked {
        skyward_sys::kmsg(&format!("the check of {} ended with {}", mounted.mapped.display(), checked.status))?;
        if checked.status != 0 {
            for line in checked.said.lines() {
                skyward_sys::kmsg(line)?;
            }
        }
    }
    skyward_sys::kmsg(&format!("{} holds {} at {}", mounted.mapped.display(), mounted.filesystem, skyward_crypto::data::MOUNT))
}

fn show_closed(app: &mut App, closed: &[Closed]) -> Result<()> {
    use skyward_crypto::user::Lock;
    skyward_sys::kmsg(&format!("the users whose own files are still closed: {:?}", closed.iter().map(|c| c.user).collect::<Vec<_>>()))?;
    for c in closed {
        if let Some(why) = &c.why {
            skyward_sys::kmsg(&format!("the files of user {} stay closed: {why}", c.user))?;
        }
    }
    let waiting = closed.iter().find(|c| !c.made_up).map(|c| {
        let keys = match c.lock {
            Lock::PATTERN => Keys::Pattern,
            Lock::PIN => Keys::Digits,
            _ => Keys::Letters,
        };
        (c.user, keys)
    });
    app.data_waiting(waiting);
    Ok(())
}

fn read_folder(path: &Path) -> Result<Vec<FileRow>> {
    let gone = |e: &std::io::Error| e.kind() == std::io::ErrorKind::NotFound;
    let mut rows = Vec::new();
    for entry in fs::read_dir(path).with_context(|| format!("read {}", path.display()))? {
        let full = entry.with_context(|| format!("read {}", path.display()))?.path();
        let meta = match fs::symlink_metadata(&full) {
            Ok(meta) => meta,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(e).with_context(|| format!("stat {}", full.display())),
        };
        let kind = if meta.file_type().is_symlink() {
            let target = fs::read_link(&full).with_context(|| format!("read the link {}", full.display()))?;
            let folder = match fs::metadata(&full) {
                Ok(to) => to.is_dir(),
                Err(e) if gone(&e) => false,
                Err(e) => return Err(e).with_context(|| format!("follow {}", full.display())),
            };
            FileKind::Link { target: target.to_string_lossy().into_owned(), folder }
        } else if meta.is_dir() {
            let items = match fs::read_dir(&full) {
                Ok(inside) => Some(inside.count()),
                Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => None,
                Err(e) if gone(&e) => continue,
                Err(e) => return Err(e).with_context(|| format!("read {}", full.display())),
            };
            FileKind::Folder { items }
        } else if meta.is_file() {
            FileKind::File
        } else {
            FileKind::Special
        };
        let name = full.file_name().with_context(|| format!("{} has no name", full.display()))?.to_string_lossy().into_owned();
        rows.push(FileRow { name, kind, size: meta.len(), modified: meta.mtime(), mode: meta.mode(), uid: meta.uid(), gid: meta.gid() });
    }
    rows.sort_by_key(|r| (!matches!(r.kind, FileKind::Folder { .. } | FileKind::Link { folder: true, .. }), r.name.to_lowercase()));
    Ok(rows)
}

fn read_mounts(services: &[Service]) -> Result<Vec<skyward_ui::app::MountRow>> {
    use skyward_ui::app::MountRow;
    let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
    let shown = skyward_vab::mounted_partitions(slot)?;
    let holders = skyward_block::mount::holders()?;
    let props = skyward_info::installed_props(slot)?;
    let services: Vec<&Service> = services.iter().filter(|s| s.runs_on(&props)).collect();
    let mut rows = Vec::new();
    for base in skyward_vab::slot_bases(slot)? {
        let point = skyward_vab::mount_point(&base);
        let name = format!("{base}{}", slot.suffix());
        let mounted = shown.iter().find(|m| m.partition == name);
        let mut using = BTreeSet::new();
        if let Some(m) = mounted {
            using.extend(holders.get(&skyward_block::devnum(&m.device)?).into_iter().flatten().cloned());
            for s in services.iter().filter(|s| !s.oneshot && s.uses(&point)) {
                let program = Path::new(&s.path).file_name().with_context(|| format!("{} names no program", s.path))?;
                using.insert(program.to_string_lossy().into_owned());
            }
        }
        rows.push(MountRow {
            base,
            point: point.display().to_string(),
            mounted: mounted.is_some(),
            holders: using.into_iter().collect(),
        });
    }
    Ok(rows)
}

fn read_slots() -> Result<skyward_vab::BootSlots, String> {
    let read = || -> Result<skyward_vab::BootSlots> {
        let (binder, hardware) = (skyward_binder::driver::Binder::open()?, skyward_binder::driver::Binder::hardware()?);
        let control = skyward_vab::boot::BootControl::open(&binder, &hardware)?;
        control.slots()
    };
    read().map_err(|e| format!("{e:#}"))
}

fn set_slot(slot: skyward_vab::Slot) -> Result<skyward_vab::BootSlots, String> {
    let set = || -> Result<skyward_vab::BootSlots> {
        let state = skyward_vab::snapshot_state()?;
        ensure!(state == skyward_vab::SnapshotState::None, "an update is in flight ({state:?}), and it set the slot it needs");
        skyward_systems::install::ensure_installed()?;
        let (binder, hardware) = (skyward_binder::driver::Binder::open()?, skyward_binder::driver::Binder::hardware()?);
        let control = skyward_vab::boot::BootControl::open(&binder, &hardware)?;
        control.set_active(slot)?;
        skyward_sys::kmsg(&format!("slot {} starts next", slot.letter()))?;
        control.slots()
    };
    set().map_err(|e| format!("{e:#}"))
}

fn detach(services: Vec<Service>) -> Detach {
    Box::new(move || {
        let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
        let mounted = skyward_vab::mounted_partitions(slot)?;
        let busy = services::pin(&services, &mounted)?;
        skyward_vab::release_partitions(slot, &busy)
    })
}

fn install_options(app: &App) -> Options {
    Options { keep: app.state.keep_recovery && app.state.recovery_partitions, signed: app.state.check_signature }
}

const SCAN_EVERY: Duration = Duration::from_secs(10);

fn wlan_worker(w: &Option<wlan::Worker>) -> Result<&wlan::Worker> {
    w.as_ref().context("the phone's WLAN is not brought up")
}

fn installed(cfg: &DeviceConfig, info: &skyward_info::DeviceInfo) -> Result<Installed> {
    let slot = info.slot.context("only an A/B phone is updated")?;
    let props = skyward_info::installed_props(slot)?;
    let prop = |name: &str| props.get(name).cloned().with_context(|| format!("the installed system says no {name}"));
    Ok(Installed {
        device: prop("ro.product.mod_device")?,
        version: skyward_info::detect_os_version(&props, &cfg.device.os_version_prop)
            .with_context(|| format!("the installed system says no {}", cfg.device.os_version_prop))?,
        android: prop("ro.build.version.release")?,
        unlocked: skyward_block::boot_param("androidboot.flash.locked")?.as_deref() == Some("0"),
    })
}

fn downloaded(release: &Release) -> PathBuf {
    Path::new(skyward_crypto::data::SDCARD).join(DOWNLOADS).join(&release.name)
}

fn have(app: &App, release: &Release) -> Have {
    if !app.state.data.storage_open() {
        return Have::Nothing;
    }
    let path = downloaded(release);
    let size = |p: &Path| match fs::metadata(p) {
        Ok(m) => Some(m.len()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            let _ = skyward_sys::kmsg(&format!("read {}: {e}", p.display()));
            None
        }
    };
    match (size(&path), size(&download::partial(&path))) {
        (Some(whole), _) => Have::Whole(whole),
        (None, Some(part)) => Have::Part(part),
        (None, None) => Have::Nothing,
    }
}

fn wait(fds: &[i32], timeout_ms: i32) -> Result<()> {
    let mut polls: Vec<libc::pollfd> = fds.iter().map(|&fd| libc::pollfd { fd, events: libc::POLLIN, revents: 0 }).collect();
    let n = unsafe { libc::poll(polls.as_mut_ptr(), polls.len() as libc::nfds_t, timeout_ms) };
    if n < 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err).context("poll input devices");
        }
    }
    Ok(())
}

fn job_end(end: filework::End) -> JobEnd {
    match end {
        filework::End::Done => JobEnd::Done,
        filework::End::Stopped => JobEnd::Stopped,
        filework::End::Failed(why) => JobEnd::Failed(why),
    }
}

pub fn run(cfg: &DeviceConfig, programs: &[Service], font: &Path, mono: &Path, request: bcb::Request, start: Screen, dump: Option<&Path>) -> Result<()> {
    let mut pages = Pages::open(cfg, programs, font, mono, request, start, dump)?;
    pages.begin()?;
    let sleeper = Sleeper::start()?;
    let uevents = uevent::Listener::open()?;
    loop {
        pages.turn(&sleeper, &uevents)?;
    }
}

struct Pages<'a> {
    cfg: &'a DeviceConfig,
    programs: &'a [Service],
    request: bcb::Request,
    dump: Option<&'a Path>,
    app: App,
    info: skyward_info::DeviceInfo,
    unread: Untaken,
    fonts: Fonts,
    surface: KmsSurface,
    screen: Canvas,
    inputs: Inputs,
    started: Instant,
    touch_known: bool,
    control: Option<Control>,
    haptics: Option<Haptics>,
    backlight: Backlight,
    keeper: Keeper,
    shown_clock: String,
    arriving: Option<Incoming>,
    readings: Readings,
    read_at: Instant,
    asked: Option<Instant>,
    blanked: bool,
    rested_at: Option<Instant>,
    fastboot: bool,
    port_at: Instant,
    transfer: Option<Transfer>,
    sideloading: bool,
    misc: PathBuf,
    kept: bool,
    leave: Target,
    leave_at: Option<u64>,
    installer: Option<Installer>,
    pending: Option<Pending>,
    rescue_leaves: Option<(Target, bool)>,
    eraser: Option<Eraser>,
    erase_leaves: bool,
    rescue_erasing: bool,
    decryptor: Option<Decryptor>,
    closed: Vec<Closed>,
    part_worker: Option<partwork::Worker>,
    worker: Option<filework::Worker>,
    relabeler: Option<filework::Worker>,
    backuper: Option<backup::Worker>,
    merger: Option<merge::Merger>,
    systems_worker: Option<systems::Worker>,
    switch_asked: bool,
    switched: bool,
    servers: Servers,
    wlan: Option<wlan::Worker>,
    scanned_at: Instant,
    checker: Option<updates::Worker>,
    downloader: Option<updates::Worker>,
    skyward_checker: Option<updates::Worker>,
    skyward_updater: Option<updates::Worker>,
    shell: Option<pty::Shell>,
    plugged: Option<Plugged>,
    seen: Option<String>,
    drive_at: Instant,
}

impl<'a> Pages<'a> {
    fn open(cfg: &'a DeviceConfig, programs: &'a [Service], font: &Path, mono: &Path, request: bcb::Request, start: Screen, dump: Option<&'a Path>) -> Result<Pages<'a>> {
        if cfg.display.density == 0 {
            return Err(anyhow!("{}: [display] density is required to lay out the UI", cfg.device.codename));
        }
        let mut unread = Untaken::default();
        let battery = skyward_info::battery_now();
        untaken(&mut unread.battery, "the battery", battery.as_ref().err())?;
        let info = skyward_info::collect_with(cfg, battery.unwrap_or_default())?;
        let fonts = Fonts::load(font, 0, mono)?;
        let mut surface = KmsSurface::open("/dev/dri/card0")?;
        let (w, h) = surface.size();
        if skyward_sys::runs_again() {
            surface.rest()?;
            surface.present(&Canvas::new(w, h))?;
            surface.settle()?;
        }
        eprintln!("display {}", surface.describe());
        let inputs = Inputs::open(w, h, cfg.touch.writes.clone())?;
        let started = Instant::now();
        let control = Control::open()?;
        let haptics = cfg.haptics.as_ref().map(Haptics::open).transpose()?;
        let backlight = Backlight::open()?;
        let first = match &request.work {
            Work::Fastboot => Screen::Fastboot,
            Work::PromptWipeData => Screen::CantStart,
            Work::Unsupported(_) => Screen::Unhandled,
            Work::Menu
            | Work::Exit
            | Work::Update(_)
            | Work::WipeData { .. }
            | Work::WipeCache { .. }
            | Work::WipeAb { .. }
            | Work::Sideload { .. }
            | Work::Rescue
            | Work::UnshareBlocks
            | Work::SwitchSystem { .. }
            | Work::SystemStopped => start,
        };
        let mut app = App::new(first, w, h, cfg.display.density as f32 / 160.0);
        app.set_screen_corners(cfg.display.corner_radius as f32);
        let kept = settings::read()?;
        let locale = request.locale.as_deref().or(info.locale.as_deref());
        let named = locale.map(Lang::for_locale).transpose()?;
        app.take_settings(kept.settings(), named);
        if !app.state.language_chosen && matches!(app.state.lang, Lang::En) {
            app.state.lang = Lang::PtBr;
            app.refresh();
        }
        if kept.settings().is_none() {
            app.state.brightness = 0.55;
        }
        let keeper = Keeper::new(&kept);
        skyward_sys::kmsg(&format!(
            "the pages speak {:?}, {}, and the locale named is {}",
            app.state.lang,
            if app.state.language_chosen { "the language chosen" } else { "with none chosen" },
            locale.unwrap_or("none")
        ))?;
        if info.slot.is_some() {
            app.set_boot_slots(read_slots());
        }
        app.set_asked(asked(&request));
        let screen = Canvas::new(w, h);
        Canvas::keep_ready(w, h, 3);
        let readings = Readings::start(cfg)?;
        let read_at = Instant::now();
        let fastboot = first == Screen::Fastboot;
        let port_at = Instant::now();
        let mut transfer: Option<Transfer> = None;
        if app.state.file_transfer {
            match Transfer::start(cfg, &info, app.state.time_zone) {
                Ok(t) => transfer = Some(t),
                Err(e) => {
                    skyward_sys::kmsg(&format!("file transfer did not start: {e:#}"))?;
                    app.file_transfer_failed(false, &skyward_ui::fmt::transfer_failed(app.state.lang, &format!("{e:#}")), monotonic_ms());
                }
            }
        }
        if fastboot {
            if let Err(e) = skyward_sys::usb::switch("fastboot", USB_TIMEOUT) {
                skyward_sys::kmsg(&format!("fastbootd has not taken the port: {e:#}"))?;
                app.report(&format!("{e:#}"), monotonic_ms());
            }
            let (running, link) = fastboot_status()?;
            app.set_fastboot(running, link);
        }
        app.state.cache_partition = match wipe::cache() {
            Ok(cache) => cache.is_some(),
            Err(e) => {
                skyward_sys::kmsg(&format!("whether the phone has a cache partition is not known: {e:#}"))?;
                false
            }
        };
        app.state.phone_logs = cfg.wipe.logs.len();
        let by_name = Path::new(skyward_block::BY_NAME);
        let both = |base: &str| -> Result<bool> { Ok(by_name.join(format!("{base}_a")).try_exists()? && by_name.join(format!("{base}_b")).try_exists()?) };
        app.state.recovery_partitions = both("recovery")? || both("boot")?;
        app.state.dual_boot = both("recovery")? && both("boot")? && both("init_boot")?;
        let systems_worker = match app.state.dual_boot {
            true => Some(systems::Worker::start(cfg.device.os_version_prop.clone())?),
            false => None,
        };
        if fastboot && app.state.dual_boot {
            let metadata = Path::new("/metadata");
            let mounted = mount::is_mount_point(metadata)?;
            let put_back = skyward_systems::install::ensure_installed();
            if !mounted && mount::is_mount_point(metadata)? {
                mount::umount(metadata)?;
            }
            if let Err(e) = put_back {
                skyward_sys::kmsg(&format!("the installed system's boot set did not go back before fastboot: {e:#}"))?;
                app.report(&format!("{e:#}"), monotonic_ms());
            }
        }
        Ok(Pages {
            cfg,
            programs,
            dump,
            app,
            info,
            unread,
            fonts,
            surface,
            screen,
            inputs,
            started,
            touch_known: false,
            control,
            haptics,
            backlight,
            keeper,
            shown_clock: String::new(),
            arriving: Some(Incoming::Arrived),
            readings,
            read_at,
            asked: None,
            blanked: false,
            rested_at: None,
            fastboot,
            port_at,
            transfer,
            sideloading: false,
            misc: bcb::misc(),
            kept: request.work.lasts(),
            leave: if request.shutdown_after { Target::PowerOff } else { Target::System },
            leave_at: None,
            request,
            installer: None,
            pending: None,
            rescue_leaves: None,
            eraser: None,
            erase_leaves: false,
            rescue_erasing: false,
            decryptor: None,
            closed: Vec::new(),
            part_worker: None,
            worker: None,
            relabeler: None,
            backuper: None,
            merger: None,
            systems_worker,
            switch_asked: false,
            switched: false,
            servers: Arc::new(Mutex::new(Vec::new())),
            wlan: None,
            scanned_at: Instant::now(),
            checker: None,
            downloader: None,
            skyward_checker: None,
            skyward_updater: None,
            shell: None,
            plugged: None,
            seen: None,
            drive_at: Instant::now(),
        })
    }

    fn begin(&mut self) -> Result<()> {
        let cfg = self.cfg;
        match &self.request.work {
            Work::WipeData { reformat } => {
                self.eraser = Some(factory_reset(&mut self.app, self.request.keep_memtag_mode, true, reformat.clone())?);
                self.erase_leaves = true;
            }
            Work::WipeCache { reformat } => {
                self.eraser = Some(parts_wipe(&mut self.app, vec![Step::Cache], &cfg.wipe.logs, true, reformat.clone())?);
                self.erase_leaves = true;
            }
            Work::Sideload { .. } => {
                self.app.start_sideload();
                self.installer = Some(Installer::start(detach(self.programs.to_vec()), install_options(&self.app))?);
                self.sideloading = true;
            }
            Work::Rescue => {
                self.app.start_rescue();
                self.installer = Some(Installer::rescue(detach(self.programs.to_vec()), install_options(&self.app))?);
                self.sideloading = true;
            }
            Work::Update(update) => {
                self.app.start_asked(update.security, update.package.clone());
                let retry = update.retry > 0;
                if !retry && panicked() {
                    skyward_sys::kmsg("the phone came here after its kernel stopped, so the update asked for is not installed")?;
                    self.app.install(skyward_install::Event::Finished(skyward_install::Outcome::Refused(skyward_install::Refusal::Panicked)), monotonic_ms());
                    bcb::clear(&self.misc)?;
                    self.kept = false;
                } else {
                    let (package, needs) = asked::locate(&update.package, self.app.state.cache_partition);
                    skyward_sys::kmsg(&format!("the update asked for is {package:?}, which needs {needs:?}"))?;
                    let wanted = Pending { package, needs, retry };
                    match needs {
                        Needs::Data | Needs::User(_) => {
                            self.app.asked_opening();
                            self.decryptor = Some(Decryptor::start(decrypt::Job::Open)?);
                            self.pending = Some(wanted);
                        }
                        Needs::Nothing | Needs::Cache => self.installer = Some(install_asked(self.programs, &self.app, wanted)?),
                    }
                }
            }
            Work::WipeAb { package_size } => {
                self.app.part_work_started(PartKind::Disable, true);
                self.part_worker = Some(partwork::Worker::start(partwork::Task::Disable { package_size: *package_size })?);
            }
            Work::UnshareBlocks => {
                let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
                self.app.part_work_started(PartKind::Unshare, true);
                self.part_worker = Some(partwork::Worker::start(partwork::Task::Unshare { slot })?);
            }
            Work::SwitchSystem { to } => match self.systems_worker.as_mut() {
                Some(worker) => {
                    self.app.switch_asked(if to == "installed" { Who::Installed } else { Who::Other });
                    worker.send(systems::Task::SwitchNamed(to.clone()));
                    self.switch_asked = true;
                }
                None => {
                    skyward_sys::kmsg(&format!("asked to switch to the system {to:?}, which this phone cannot hold"))?;
                    bcb::clear(&self.misc)?;
                    self.kept = false;
                }
            },
            Work::SystemStopped => {
                let why = self.request.reason.clone().unwrap_or_default();
                skyward_sys::kmsg(&format!("the second system's start stopped: {why}"))?;
                if self.app.state.dual_boot {
                    self.app.system_stopped(why);
                }
            }
            Work::Menu | Work::Exit | Work::Fastboot | Work::PromptWipeData | Work::Unsupported(_) => {}
        }
        if let (Some(worker), false, false) = (self.systems_worker.as_mut(), self.switch_asked, self.fastboot) {
            worker.send(systems::Task::Resume);
        }
        if self.transfer.is_some()
            && !self.fastboot
            && !self.sideloading
            && let Err(e) = skyward_sys::usb::switch(skyward_sys::usb::resting(), USB_TIMEOUT)
        {
            let _ = skyward_sys::usb::set("adb", USB_TIMEOUT);
            skyward_sys::kmsg(&format!("the port did not take file transfer: {e:#}"))?;
            self.app.report(&format!("{e:#}"), monotonic_ms());
        }
        if let Some(slot) = self.info.slot {
            let standing = skyward_vab::standing(slot)?;
            if matches!(standing, Standing::Merging(_)) && skyward_vab::snapshots_mapped(slot)? {
                skyward_sys::kmsg("the update's merge goes on")?;
                self.merger = Some(merge::Merger::start(slot, false)?);
            }
            self.app.set_update(standing);
        }
        if let Some(w) = &cfg.wlan {
            self.app.state.wlan = Some(Wlan::default());
            let modules = cfg.modules.sources.iter().map(|s| skyward_modules::source_dir(s)).collect::<Result<Vec<_>>>()?;
            let libraries = secure::libraries().unwrap_or_else(|e| {
                let _ = skyward_sys::kmsg(&format!("the vendor interface's libraries are not open: {e:#}"));
                None
            });
            self.wlan = Some(wlan::Worker::start(w.clone(), modules, libraries, self.servers.clone())?);
        }
        self.scanned_at = Instant::now();
        self.app.state.update_server = cfg.update.is_some();
        self.drive_at = Instant::now();
        Ok(())
    }

    fn turn(&mut self, sleeper: &Sleeper, uevents: &uevent::Listener) -> Result<()> {
        let now = monotonic_ms();
        if self.leave_at.is_some_and(|at| now >= at) {
            self.leave_at = None;
            power(self.leave)?;
        }
        if !self.touch_known && self.started.elapsed() >= TOUCH_WAIT {
            self.touch_known = true;
            if !self.inputs.has_panel() {
                skyward_sys::kmsg("no touch panel came up; the pages are worked with the keys")?;
                self.app.touchscreen_missing();
            }
        }
        self.tick(now)?;
        self.take_readings(now)?;
        if self.fastboot && self.port_at.elapsed().as_secs() >= 1 {
            let (running, link) = fastboot_status()?;
            self.app.set_fastboot(running, link);
            self.port_at = Instant::now();
        }
        let (lit, shown) = self.show(now)?;
        self.wait(sleeper, uevents, now, lit, shown)?;
        let now = monotonic_ms();
        for slept in sleeper.slept() {
            match slept {
                Slept::Woke => skyward_sys::kmsg("the phone slept and woke")?,
                Slept::Refused(why) => skyward_sys::kmsg(&format!("the phone did not sleep: {why}"))?,
            }
        }
        if uevents.heard()? {
            self.readings.ask();
            self.asked = Some(Instant::now());
        }
        let mut drive_told = None;
        if self.drive_at.elapsed().as_secs() >= 1 {
            drive_told = look_for_drive(&mut self.plugged, &mut self.seen)?;
            self.drive_at = Instant::now();
        }
        self.installer_told(now)?;
        self.eraser_told(now)?;
        self.files_told(now)?;
        self.relabel_told(now);
        self.parts_told(now)?;
        self.backup_told(now);
        self.merge_told(now)?;
        self.systems_told(now)?;
        self.wlan_told()?;
        self.keep_settings(now)?;
        self.updates_told(now)?;
        self.skyward_told(now)?;
        self.decrypt_told(now)?;
        self.shell_told()?;
        self.take_in(drive_told, now)?;
        self.offer_storage(now)
    }

    fn tick(&mut self, now: u64) -> Result<()> {
        for fx in self.app.tick(now) {
            match fx {
                Effect::Haptic => self.click()?,
                Effect::Ticks => self.ticks()?,
                other => unreachable!("time passing asks for nothing but the motor, not {other:?}"),
            }
        }
        let time = clock(self.app.state.time_zone)?;
        if time != self.shown_clock {
            self.shown_clock = time;
            self.app.redraw();
        }
        Ok(())
    }

    fn click(&mut self) -> Result<()> {
        if let Some(h) = &mut self.haptics {
            h.click(self.app.state.vibration)?;
        }
        Ok(())
    }

    fn ticks(&mut self) -> Result<()> {
        if let Some(h) = &mut self.haptics {
            h.ticks()?;
        }
        Ok(())
    }

    fn take_readings(&mut self, now: u64) -> Result<()> {
        for reading in self.readings.taken() {
            match reading {
                Reading::Battery(battery, at) => {
                    if self.asked.is_some_and(|asked| at >= asked) {
                        self.asked = None;
                    }
                    untaken(&mut self.unread.battery, "the battery", battery.as_ref().err())?;
                    if let Ok(battery) = battery
                        && battery != self.info.battery
                    {
                        plug(&mut self.app, &self.info.battery, &battery, now)?;
                        self.info.battery = battery;
                        self.app.refresh();
                    }
                }
                Reading::All(all, at) => {
                    if self.asked.is_some_and(|asked| at >= asked) {
                        self.asked = None;
                    }
                    untaken(&mut self.unread.details, "the phone's details", all.as_ref().as_ref().err())?;
                    if let Ok(all) = *all
                        && at >= self.read_at
                        && all != self.info
                    {
                        plug(&mut self.app, &self.info.battery, &all.battery, now)?;
                        self.info = all;
                        self.app.refresh();
                    }
                }
            }
        }
        Ok(())
    }

    fn show(&mut self, now: u64) -> Result<(bool, bool)> {
        let panel = self.app.panel(now);
        let mut shown = false;
        if panel.is_some() {
            if self.app.take_dirty() || !self.surface.is_on() {
                let t0 = Instant::now();
                self.app.compose(&mut self.screen, &self.fonts, &self.info, &self.shown_clock, now);
                self.surface.present(&self.screen)?;
                eprintln!("frame {:?} in {} ms", self.app.screen(), t0.elapsed().as_millis());
                if let Some(path) = self.dump {
                    fs::write(path, &self.screen.data).with_context(|| format!("write {}", path.display()))?;
                }
                shown = true;
            }
            self.blanked = false;
        } else if !self.blanked {
            self.screen.data.fill(0);
            self.surface.present(&self.screen)?;
            self.surface.settle()?;
            self.blanked = true;
        }
        if self.backlight.level == 0 && panel.is_some() {
            self.surface.settle()?;
        }
        self.backlight.show(panel)?;
        if panel.is_some() {
            self.surface.speed_up()?;
            self.rested_at = None;
        } else if self.surface.is_on() {
            self.surface.rest()?;
            self.rested_at = Some(Instant::now());
        } else if self.rested_at.is_some_and(|at| at.elapsed() >= REST) {
            self.surface.turn_off()?;
            self.rested_at = None;
        }
        Ok((panel.is_some(), shown))
    }

    fn wait(&mut self, sleeper: &Sleeper, uevents: &uevent::Listener, now: u64, lit: bool, shown: bool) -> Result<()> {
        let ahead = lit && !shown && self.app.draw_ahead(&self.fonts, &self.info);
        let floor = if shown { 0 } else { 1 };
        let until_due = self.app.deadline(now).map(|d| d.saturating_sub(now) as i32);
        let rest_left = self.rested_at.map(|at| REST.saturating_sub(at.elapsed()).as_millis() as i32);
        let timeout = if ahead { 0 } else { until_due.into_iter().chain(rest_left).min().map_or(1000, |t| t.clamp(floor, 1000)) };
        sleeper.allow(self.may_sleep()?);
        let mut fds = vec![self.inputs.fd(), uevents.fd(), sleeper.fd()];
        if let Some(i) = &self.installer {
            fds.push(i.fd());
        }
        if let Some(c) = &self.control {
            fds.push(c.fd());
        }
        if let Some(e) = &self.eraser {
            fds.push(e.fd());
        }
        if let Some(sh) = &self.shell {
            fds.extend(sh.fds());
        }
        if let Some(w) = &self.worker {
            fds.push(w.fd());
        }
        if let Some(r) = &self.relabeler {
            fds.push(r.fd());
        }
        if let Some(b) = &self.backuper {
            fds.push(b.fd());
        }
        if let Some(m) = &self.merger {
            fds.push(m.fd());
        }
        if let Some(d) = &self.decryptor {
            fds.push(d.fd());
        }
        if let Some(p) = &self.part_worker {
            fds.push(p.fd());
        }
        if let Some(w) = &self.wlan {
            fds.push(w.fd());
        }
        if let Some(c) = &self.checker {
            fds.push(c.fd());
        }
        if let Some(d) = &self.downloader {
            fds.push(d.fd());
        }
        if let Some(c) = &self.skyward_checker {
            fds.push(c.fd());
        }
        if let Some(u) = &self.skyward_updater {
            fds.push(u.fd());
        }
        if let Some(s) = &self.systems_worker {
            fds.push(s.fd());
        }
        fds.push(self.readings.fd());
        wait(&fds, timeout)
    }

    fn may_sleep(&self) -> Result<bool> {
        let systems_busy = self.systems_worker.as_ref().is_some_and(systems::Worker::busy);
        let busy = self.installer.is_some()
            || self.eraser.is_some()
            || self.worker.is_some()
            || self.relabeler.is_some()
            || self.backuper.is_some()
            || self.merger.is_some()
            || self.decryptor.is_some()
            || self.part_worker.is_some()
            || self.checker.is_some()
            || self.downloader.is_some()
            || self.skyward_checker.is_some()
            || self.skyward_updater.is_some()
            || systems_busy;
        let joining = self.app.state.wlan.as_ref().and_then(|w| w.joined.as_ref()).is_some_and(|j| matches!(j.stage, Joining::Joining | Joining::Addressing));
        let running = match &self.shell {
            Some(sh) => !sh.waiting()?,
            None => false,
        };
        Ok(self.surface.is_off()
            && !busy
            && !self.fastboot
            && !self.sideloading
            && self.leave_at.is_none()
            && !joining
            && !running
            && self.asked.is_none()
            && usb_link()? != UsbLink::Connected)
    }

    fn done_with_request(&mut self) -> Result<()> {
        if self.kept {
            bcb::clear(&self.misc)?;
            self.kept = false;
        }
        Ok(())
    }

    fn mounts(&self) -> Result<Vec<skyward_ui::app::MountRow>, String> {
        read_mounts(self.programs).map_err(|e| format!("{e:#}"))
    }
}

impl Pages<'_> {
    fn installer_told(&mut self, now: u64) -> Result<()> {
        use skyward_install::{Event as Told, Outcome};
        let told = self.installer.as_ref().map(Installer::events).unwrap_or_default();
        for ev in told {
            let finished = matches!(ev, Told::Finished(_));
            let installed = matches!(ev, Told::Finished(Outcome::Installed { .. } | Outcome::Ran { .. }));
            let wipe_after = matches!(ev, Told::Finished(Outcome::Installed { wipe: true, .. }));
            let script_cache = matches!(ev, Told::Finished(Outcome::Ran { wipe_cache: true, .. }));
            if matches!(ev, Told::Done(Outcome::SystemInstalled { .. }) | Told::Finished(Outcome::SystemInstalled { .. })) {
                self.systems_worker.as_mut().context("a second system went in on a phone that cannot hold one")?.send(systems::Task::Read);
            }
            eprintln!("install {ev:?}");
            match &ev {
                Told::Wipe => {
                    ensure!(self.eraser.is_none(), "erasing is under way already");
                    if withhold(&mut self.transfer, &mut self.app, now)? {
                        self.eraser = Some(rescue_wipe(&mut self.app)?);
                        self.rescue_erasing = true;
                    } else {
                        self.installer.as_ref().context("rescue asked for erasing")?.wiped(false);
                    }
                }
                Told::Reboot(target) => self.rescue_leaves = Some(rescue_target(target)),
                _ => {}
            }
            self.app.install(ev, now);
            if finished {
                self.installer = None;
                self.sideloading = false;
                if matches!(self.request.work, Work::Sideload { .. } | Work::Update(_) | Work::Rescue) {
                    self.done_with_request()?;
                }
                let (leaves, cache_after) = match &self.request.work {
                    Work::Sideload { auto_reboot } => (*auto_reboot, script_cache),
                    Work::Update(update) => (installed, script_cache || (installed && update.wipe_cache)),
                    _ => (false, script_cache),
                };
                if cache_after && !self.app.state.cache_partition {
                    skyward_sys::kmsg("the install asked for the cache partition to be erased, and the phone has none")?;
                }
                if wipe_after {
                    ensure!(self.eraser.is_none(), "erasing is under way already");
                    self.eraser = Some(factory_reset(&mut self.app, false, false, None)?);
                    self.erase_leaves = leaves;
                } else if cache_after && self.app.state.cache_partition {
                    ensure!(self.eraser.is_none(), "erasing is under way already");
                    self.eraser = Some(parts_wipe(&mut self.app, vec![Step::Cache], &self.cfg.wipe.logs, false, None)?);
                    self.erase_leaves = leaves;
                } else if leaves {
                    self.leave_at = Some(now + LEAVE_AFTER_MS);
                }
                if let Some((target, again)) = self.rescue_leaves.take() {
                    if again {
                        bcb::write(&self.misc, &bcb::Bcb { command: "boot-rescue".into(), args: Vec::new() })?;
                    }
                    power(target)?;
                }
            }
        }
        Ok(())
    }

    fn eraser_told(&mut self, now: u64) -> Result<()> {
        let told = self.eraser.as_ref().map(Eraser::events).unwrap_or_default();
        let mut erased = false;
        for ev in told {
            match ev {
                wipe::Event::At(_) | wipe::Event::Progress(..) if self.rescue_erasing => {}
                wipe::Event::At(step) => self.app.erase_at(part(step)),
                wipe::Event::Progress(done, total) => self.app.erase_progress(done, total),
                wipe::Event::Ended(outcome) if self.rescue_erasing => {
                    let said = outcome.as_ref().map_or_else(wipe::said, |_| "done".to_string());
                    skyward_sys::kmsg(&format!("erasing for rescue ended: {said}"))?;
                    self.installer.as_ref().context("rescue waits for erasing to end")?.wiped(outcome.is_ok());
                    self.app.rescue_wiped(outcome.map_err(erase_stop), now);
                    if self.app.state.data == Data::Erased {
                        self.closed.clear();
                    }
                    self.rescue_erasing = false;
                    erased = true;
                }
                wipe::Event::Opened(opening) => {
                    if let Some(mounted) = &opening.mounted {
                        log_mounted(mounted)?;
                    }
                    self.closed = opening.closed;
                    show_closed(&mut self.app, &self.closed)?;
                }
                wipe::Event::Ended(outcome) => {
                    let said = outcome.as_ref().map_or_else(wipe::said, |_| "done".to_string());
                    skyward_sys::kmsg(&format!("erasing ended: {said}"))?;
                    self.done_with_request()?;
                    if outcome.is_ok() && self.app.state.erase.reset {
                        bcb::clear(&self.misc)?;
                    }
                    if outcome.is_ok() && self.erase_leaves {
                        self.leave_at = Some(now + LEAVE_AFTER_MS);
                    }
                    self.app.erase_ended(outcome.map_err(erase_stop), now);
                    if self.app.state.data == Data::Erased {
                        self.closed.clear();
                    }
                    erased = true;
                }
            }
        }
        if erased {
            self.eraser = None;
            self.keeper.remade();
        }
        Ok(())
    }

    fn files_told(&mut self, now: u64) -> Result<()> {
        let told = self.worker.as_ref().map(filework::Worker::events).unwrap_or_default();
        let mut worked = false;
        for ev in told {
            match ev {
                filework::Event::Progress(p) => self.app.file_progress((p.bytes, p.total_bytes), (p.entries, p.total_entries)),
                filework::Event::Relabeling(_) => unreachable!("work on files gives no labels"),
                filework::Event::Ended(end) => {
                    self.app.file_job_ended(job_end(end), now);
                    worked = true;
                }
            }
        }
        if worked {
            self.worker = None;
            let folder = self.app.state.files.path.clone();
            list(&mut self.app, folder)?;
        }
        Ok(())
    }

    fn relabel_told(&mut self, now: u64) {
        let told = self.relabeler.as_ref().map(filework::Worker::events).unwrap_or_default();
        let mut relabeled = false;
        for ev in told {
            match ev {
                filework::Event::Relabeling(p) => self.app.relabel_progress(p.checked, p.total, p.changed),
                filework::Event::Progress(_) => unreachable!("relabeling moves no bytes"),
                filework::Event::Ended(end) => {
                    self.app.relabel_ended(job_end(end), now);
                    relabeled = true;
                }
            }
        }
        if relabeled {
            self.relabeler = None;
        }
    }

    fn parts_told(&mut self, now: u64) -> Result<()> {
        let told = self.part_worker.as_ref().map(partwork::Worker::events).unwrap_or_default();
        let mut parted = false;
        for ev in told {
            match ev {
                partwork::Event::Listed(names) => self.app.part_listed(names),
                partwork::Event::At(index) => self.app.part_at(index),
                partwork::Event::Ended(index, item, said) => {
                    skyward_sys::kmsg(&format!("partition work: item {index} ended as {item:?}"))?;
                    self.app.part_ended(index, item_end(item), &said);
                }
                partwork::Event::Done(done) => {
                    skyward_sys::kmsg(&format!("partition work ended: {done:?}"))?;
                    let work = self.app.state.part_work.as_ref().context("partition work shows on its page")?;
                    let whole = done.is_ok() && !work.items.iter().any(|(_, s)| matches!(s, ItemState::Ended(ItemEnd::Failed(_))));
                    let data = work.items.iter().any(|(name, _)| name == DATA_PARTITION);
                    let asked = work.asked;
                    if asked {
                        self.done_with_request()?;
                        if whole {
                            self.leave_at = Some(now + LEAVE_AFTER_MS);
                        }
                    }
                    if data {
                        self.closed.clear();
                    }
                    self.app.part_work_ended(done.map_err(part_stop), now);
                    parted = true;
                }
            }
        }
        if parted {
            self.part_worker = None;
            self.keeper.remade();
        }
        Ok(())
    }

    fn backup_told(&mut self, now: u64) {
        let told = self.backuper.as_ref().map(backup::Worker::events).unwrap_or_default();
        let mut backed = false;
        for ev in told {
            match ev {
                backup::Event::Plan(plan) => self.app.backup_plan(plan),
                backup::Event::At { item, done, of, checking } => self.app.backup_at(BackupAt { item, done, of, checking }),
                backup::Event::Ended(end) => {
                    self.app.backup_ended(backup_end(end), now);
                    backed = true;
                }
            }
        }
        if backed {
            self.backuper = None;
        }
    }

    fn merge_told(&mut self, now: u64) -> Result<()> {
        let Some(m) = &self.merger else { return Ok(()) };
        let slot = self.info.slot.context("only an A/B phone merges an update")?;
        for ev in m.events() {
            match ev {
                merge::Event::Looked(look) => {
                    eprintln!("merge {look:?}");
                    self.app.set_update(match look {
                        Merge::Going(done) => Standing::Merging(done),
                        Merge::Done | Merge::NeedsReboot => Standing::Merged,
                        Merge::Failed(code) => Standing::Failed(code),
                        Merge::Cancelled | Merge::Idle(_) => skyward_vab::standing(slot)?,
                    });
                }
                merge::Event::Failed(why) => {
                    skyward_sys::kmsg(&format!("the update did not merge: {why}"))?;
                    self.app.report(&skyward_ui::fmt::merge_failed(self.app.state.lang, &why), now);
                    self.app.set_update(skyward_vab::standing(slot)?);
                }
            }
        }
        if m.ended() {
            self.merger = None;
            self.read_at = Instant::now();
            read_details(self.cfg, &mut self.info, &mut self.unread.details)?;
            self.app.refresh();
        }
        Ok(())
    }

    fn systems_told(&mut self, now: u64) -> Result<()> {
        let Some(worker) = &mut self.systems_worker else { return Ok(()) };
        for ev in worker.events() {
            let lang = self.app.state.lang;
            match ev {
                systems::Event::Resumed(to) => {
                    skyward_sys::kmsg(&format!("a switch cut off before is finished, and the {to:?} system starts next"))?;
                    self.app.report(skyward_ui::fmt::switch_resumed(lang), now);
                }
                systems::Event::NotResumed(why) => {
                    skyward_sys::kmsg(&format!("a switch cut off before is not finished: {why}"))?;
                    self.app.report(&skyward_ui::fmt::switch_unfinished(lang, &why), now);
                }
                systems::Event::Switched(to) => {
                    skyward_sys::kmsg(&format!("the {to:?} system starts next"))?;
                    if self.switch_asked {
                        self.done_with_request()?;
                    }
                    self.switch_asked = false;
                    self.switched = true;
                    power(Target::System)?;
                }
                systems::Event::Failed(why) => {
                    skyward_sys::kmsg(&format!("work on the systems failed: {why}"))?;
                    if self.switch_asked {
                        self.done_with_request()?;
                    }
                    self.switch_asked = false;
                    self.app.systems_work_failed(&why, now);
                }
                systems::Event::Read(read) => {
                    if !self.switched {
                        self.app.set_systems(read);
                    }
                }
            }
        }
        Ok(())
    }

    fn wlan_told(&mut self) -> Result<()> {
        if let Some((told, traffic)) = self.wlan.as_ref().map(wlan::Worker::told) {
            if let Some(w) = told {
                self.app.set_wlan(w);
            }
            if let Some(t) = traffic {
                self.app.set_traffic(t);
            }
        }
        let wlan_on = self.app.state.wlan.as_ref().is_some_and(|w| w.power == WlanPower::On);
        if self.app.screen() == Screen::Wlan && wlan_on && self.scanned_at.elapsed() >= SCAN_EVERY {
            self.scanned_at = Instant::now();
            wlan_worker(&self.wlan)?.send(wlan::Command::Scan);
        }
        Ok(())
    }

    fn keep_settings(&mut self, now: u64) -> Result<()> {
        let metadata_busy = self.fastboot
            || self.installer.is_some()
            || self.merger.is_some()
            || self.eraser.is_some()
            || self.backuper.is_some()
            || self.part_worker.is_some()
            || self.systems_worker.as_ref().is_some_and(systems::Worker::busy);
        if !metadata_busy
            && let Some(due) = self.keeper.due(self.app.settings(), now)
        {
            match settings::write(&due) {
                Ok(()) => {
                    skyward_sys::kmsg("the settings are kept")?;
                    self.keeper.written(due);
                }
                Err(e) => {
                    skyward_sys::kmsg(&format!("the settings were not kept: {e:#}"))?;
                    self.keeper.failed();
                }
            }
        }
        Ok(())
    }

    fn updates_told(&mut self, now: u64) -> Result<()> {
        let told = self.checker.as_ref().map(updates::Worker::events).unwrap_or_default();
        let mut checked = false;
        for ev in told {
            let updates::Event::Checked(answer) = ev else { unreachable!("asking the server downloads nothing") };
            let have = match &answer {
                Ok(Some(release)) => have(&self.app, release),
                _ => Have::Nothing,
            };
            self.app.update_checked(answer, have);
            checked = true;
        }
        if checked {
            self.checker = None;
        }
        let told = self.downloader.as_ref().map(updates::Worker::events).unwrap_or_default();
        let mut fetched = false;
        for ev in told {
            match ev {
                updates::Event::Progress(progress, rate) => self.app.download_progress(progress, rate),
                updates::Event::Ended(end) => {
                    let release = self.app.state.newest().context("a download is of the newest system")?.clone();
                    let end = match end {
                        updates::End::Downloaded => DownloadEnd::Downloaded,
                        updates::End::Stopped => DownloadEnd::Stopped,
                        updates::End::Failed(why) => DownloadEnd::Failed(why),
                        updates::End::Updated(_) | updates::End::NotWritten(_) | updates::End::WriteFailed(..) => {
                            unreachable!("a system's full package is only downloaded")
                        }
                    };
                    skyward_sys::kmsg(&format!("downloading {} ended: {end:?}", release.name))?;
                    let have = have(&self.app, &release);
                    self.app.download_ended(end, have, now);
                    fetched = true;
                }
                updates::Event::Checked(_) | updates::Event::SkywardChecked(_) | updates::Event::Writing { .. } => {
                    unreachable!("a download asks nothing of its own, and writes nothing")
                }
            }
        }
        if fetched {
            self.downloader = None;
        }
        Ok(())
    }

    fn skyward_told(&mut self, now: u64) -> Result<()> {
        let told = self.skyward_checker.as_ref().map(updates::Worker::events).unwrap_or_default();
        let mut asked = false;
        for ev in told {
            let updates::Event::SkywardChecked(answer) = ev else { unreachable!("asking GitHub downloads nothing") };
            skyward_sys::kmsg(&format!("GitHub told of Skyward's newest release: {answer:?}"))?;
            self.app.skyward_checked(answer);
            asked = true;
        }
        if asked {
            self.skyward_checker = None;
        }
        let told = self.skyward_updater.as_ref().map(updates::Worker::events).unwrap_or_default();
        let mut updated = false;
        for ev in told {
            match ev {
                updates::Event::Progress(progress, rate) => self.app.skyward_progress(SkywardStage::Download(progress), rate),
                updates::Event::Writing { done, total } => self.app.skyward_progress(SkywardStage::Write { done, total }, 0),
                updates::Event::Ended(end) => {
                    let end = match end {
                        updates::End::Updated(slots) => SkywardEnd::Updated(slots),
                        updates::End::Stopped => SkywardEnd::Stopped,
                        updates::End::Failed(why) => SkywardEnd::Download(why),
                        updates::End::NotWritten(why) => SkywardEnd::NotWritten(why),
                        updates::End::WriteFailed(slots, why) => SkywardEnd::WriteFailed(slots, why),
                        updates::End::Downloaded => unreachable!("Skyward's build is written once it is downloaded"),
                    };
                    skyward_sys::kmsg(&format!("updating Skyward ended: {end:?}"))?;
                    self.app.skyward_ended(end, now);
                    updated = true;
                }
                updates::Event::Checked(_) | updates::Event::SkywardChecked(_) => unreachable!("an update asks nothing of its own"),
            }
        }
        if updated {
            self.skyward_updater = None;
        }
        Ok(())
    }

    fn decrypt_told(&mut self, now: u64) -> Result<()> {
        let Some(ended) = self.decryptor.as_ref().and_then(Decryptor::ended) else { return Ok(()) };
        self.decryptor = None;
        match ended {
            decrypt::Ended::Opened(Ok(opening)) => {
                if let Some(mounted) = &opening.mounted {
                    log_mounted(mounted)?;
                }
                self.closed = opening.closed;
                show_closed(&mut self.app, &self.closed)?;
            }
            decrypt::Ended::Opened(Err(why)) => {
                skyward_sys::kmsg(&format!("the data partition stays closed: {why}"))?;
                if self.pending.take().is_some() {
                    let failed = skyward_install::Outcome::Failed(skyward_install::Failure::Unreadable(why.clone()));
                    self.app.install(skyward_install::Event::Finished(failed), now);
                    self.done_with_request()?;
                }
                self.app.data_failed(why);
            }
            decrypt::Ended::Unlocked(Ok(still)) => {
                self.closed = still;
                show_closed(&mut self.app, &self.closed)?;
            }
            decrypt::Ended::Unlocked(Err(why)) => {
                skyward_sys::kmsg(&format!("what was entered did not open the user's files: {why}"))?;
                self.app.unlock_failed(why, now);
            }
        }
        if self.app.state.place_open(Place::Storage)
            && let Err(e) = keep_held_screenshots()
        {
            let why = format!("{e:#}");
            skyward_sys::kmsg(&format!("the screenshots held did not go into the gallery's: {why}"))?;
            self.app.screenshots_not_kept(&why, now);
        }
        if let Some(wanted) = self.pending.take() {
            match wanted.needs {
                Needs::User(user) if self.closed.iter().any(|c| c.user == user) => {
                    self.app.asked_locked(user, now);
                    self.pending = Some(wanted);
                }
                _ => self.installer = Some(install_asked(self.programs, &self.app, wanted)?),
            }
        }
        Ok(())
    }

    fn shell_told(&mut self) -> Result<()> {
        let Some(sh) = &mut self.shell else { return Ok(()) };
        let wrote = sh.read()?;
        if !wrote.is_empty() {
            self.app.term_output(&wrote);
        }
        if let Some(status) = sh.ended()? {
            let last = sh.read()?;
            if !last.is_empty() {
                self.app.term_output(&last);
            }
            self.app.term_ended(pty::how(status));
            self.shell = None;
        }
        Ok(())
    }

    fn offer_storage(&mut self, now: u64) -> Result<()> {
        let kept = match &mut self.transfer {
            Some(t) if !t.service.running() => Err(anyhow!("the file transfer service ended")),
            Some(t) => {
                let offer = self.app.state.data.storage_open() && self.eraser.is_none() && self.installer.is_none();
                t.offer(offer, self.app.state.lang).and_then(|()| {
                    if t.zone != self.app.state.time_zone {
                        t.service.set_zone(skyward_ui::fmt::rules(self.app.state.time_zone).clone())?;
                        t.zone = self.app.state.time_zone;
                    }
                    Ok(())
                })
            }
            None => Ok(()),
        };
        if let Err(e) = kept {
            let why = match self.transfer.take().expect("only file transfer that runs fails").stop(!self.fastboot && !self.sideloading) {
                Err(cause) => cause,
                Ok(()) => e,
            };
            skyward_sys::kmsg(&format!("file transfer stops: {why:#}"))?;
            self.app.file_transfer_failed(false, &skyward_ui::fmt::transfer_stopped(self.app.state.lang, &format!("{why:#}")), now);
        }
        Ok(())
    }
}

impl Pages<'_> {
    fn take_in(&mut self, drive_told: Option<Option<UsbDrive>>, now: u64) -> Result<()> {
        let requests = match &self.control {
            Some(c) => c.requests()?,
            None => Vec::new(),
        };
        let incoming = self
            .arriving
            .take()
            .into_iter()
            .chain(drive_told.map(Incoming::Drive))
            .chain(requests.into_iter().map(Incoming::Asked))
            .chain(self.inputs.read()?.into_iter().map(Incoming::Input));
        for came in incoming {
            let effects = match came {
                Incoming::Arrived => self.app.arrive(),
                Incoming::Drive(drive) => self.app.set_usb_drive(drive),
                Incoming::Asked(asked) => {
                    skyward_sys::kmsg(&format!("asked on the recovery socket: {asked:?}"))?;
                    answer(asked, self.fastboot)
                }
                Incoming::Input(Event::Touch(t, at)) => self.app.touch(t, &self.fonts, at),
                Incoming::Input(Event::Key(k, true)) => self.app.key(k, now),
                Incoming::Input(Event::Key(k, false)) => self.app.key_up(k, now),
            };
            let mut queue = VecDeque::from(effects);
            while let Some(fx) = queue.pop_front() {
                eprintln!("effect {fx:?}");
                queue.extend(self.effect(fx, now)?);
            }
        }
        Ok(())
    }

    fn effect(&mut self, fx: Effect, now: u64) -> Result<Vec<Effect>> {
        let cfg = self.cfg;
        match fx {
            Effect::Reboot(target) => {
                self.done_with_request()?;
                power(target)?
            }
            Effect::Dismiss => self.done_with_request()?,
            Effect::FactoryReset => self.reset_asked(now)?,
            Effect::Wipe(chosen) => self.wipe(chosen, now)?,
            Effect::Install => self.installer.as_ref().context("no installer holds an update")?.install(),
            Effect::StartShell(folder) => self.start_shell(&folder, now)?,
            Effect::TermInput(bytes) => {
                let sh = self.shell.as_mut().context("keys went to a shell that has ended")?;
                if let Err(e) = sh.write(&bytes) {
                    self.app.report(&format!("{e:#}"), now);
                }
            }
            Effect::ShellTo(folder) => self.shell_to(&folder, now)?,
            Effect::Haptic => self.click()?,
            Effect::Ticks => self.ticks()?,
            Effect::Screenshot { storage } => {
                let t0 = Instant::now();
                let saved = save_screenshot(&self.screen, storage, self.app.state.time_zone).map_err(|e| format!("{e:#}"));
                eprintln!("screenshot {saved:?} in {} ms", t0.elapsed().as_millis());
                self.app.screenshot_saved(saved, now);
            }
            Effect::Sideload => {
                if self.may_install(now)? {
                    self.installer = Some(Installer::start(detach(self.programs.to_vec()), install_options(&self.app))?);
                    self.sideloading = true;
                }
            }
            Effect::InstallFile(place, path) => {
                if self.may_install(now)? {
                    self.installer = Some(Installer::open(in_place(place, &path), detach(self.programs.to_vec()), install_options(&self.app))?);
                }
            }
            Effect::InstallPath(path) => {
                if self.may_install(now)? {
                    self.installer = Some(Installer::open(PathBuf::from(path), detach(self.programs.to_vec()), install_options(&self.app))?);
                }
            }
            Effect::Flash { partition, pick } => {
                let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
                self.installer.as_ref().context("no installer holds an image")?.flash(partition, pick.slots(slot));
            }
            Effect::Cancel => self.cancel_install(now)?,
            Effect::ReadMounts => self.app.set_mounts(self.mounts()),
            Effect::ReadSlots => self.app.set_boot_slots(read_slots()),
            Effect::SetSlot(slot) => self.app.slot_switched(set_slot(slot), now),
            Effect::ReadSystems => self.systems()?.send(systems::Task::Read),
            Effect::SwitchSystem(to) => {
                if self.installer.is_some() {
                    self.app.systems_work_failed(self.app.state.lang.pick(INSTALL_UNFINISHED), now);
                } else {
                    skyward_sys::kmsg(&format!("switching to the {to:?} system, as the systems page asked"))?;
                    self.systems()?.send(systems::Task::Switch(to));
                }
            }
            Effect::RemoveSystem => {
                if self.installer.is_some() {
                    self.app.systems_work_failed(self.app.state.lang.pick(INSTALL_UNFINISHED), now);
                } else {
                    skyward_sys::kmsg("removing the second system, as the systems page asked")?;
                    self.systems()?.send(systems::Task::Remove);
                }
            }
            Effect::InstallSystem { size } => self.installer.as_ref().context("no installer holds a system")?.install_system(size),
            Effect::OpenData => {
                ensure!(self.decryptor.is_none(), "the data partition is being opened already");
                self.decryptor = Some(Decryptor::start(decrypt::Job::Open)?);
            }
            Effect::Unlock { user, code } => {
                ensure!(self.decryptor.is_none(), "the data partition is being opened already");
                let job = decrypt::Job::Unlock { closed: self.closed.clone(), user, credential: code.into_bytes() };
                self.decryptor = Some(Decryptor::start(job)?);
            }
            Effect::ReadDir(folder) => list(&mut self.app, folder)?,
            Effect::ReadPlace(place, folder) => list_place(&mut self.app, place, folder)?,
            Effect::Paste { op, path, into, clash } => self.paste(op, path, into, clash, now)?,
            Effect::Delete(path) => {
                ensure!(self.worker.is_none(), "work on files is under way already");
                self.worker = Some(filework::Worker::start(filework::Task::Delete { path: PathBuf::from(&path) })?);
                self.app.file_job_started(FileTask::Delete { path });
            }
            Effect::StopJob => self.worker.as_ref().context("there is no work on files to stop")?.stop(),
            Effect::Relabel => self.relabel(now)?,
            Effect::StopRelabel => self.relabeler.as_ref().context("there is no relabeling to stop")?.stop(),
            Effect::ReadFilesystems => self.app.set_filesystems(read_filesystems().map_err(|e| format!("{e:#}"))),
            Effect::Filesystem { name, action } => self.filesystem(&name, action, now)?,
            Effect::Backup { parts, place, compress, verify } => self.back_up(parts, place, compress, verify, now)?,
            Effect::Restore { place, name, parts } => self.restore(place, &name, parts, now)?,
            Effect::FindBackups { storage, usb } => find_backups(&mut self.app, storage, usb, now),
            Effect::DeleteBackup { place, name } => {
                let deleted = delete_backup(place, &backups_in(place).join(&name)).map_err(|e| format!("{e:#}"));
                return Ok(self.app.backup_deleted(deleted, now));
            }
            Effect::StopBackup => self.backuper.as_ref().context("there is no backup or restore to stop")?.stop(),
            Effect::ReadUpdate => {
                let slot = self.info.slot.context("only an A/B phone has an update to merge")?;
                if self.merger.is_none() {
                    self.app.set_update(skyward_vab::standing(slot)?);
                }
                self.app.set_boot_slots(read_slots());
            }
            Effect::Merge => {
                ensure!(self.merger.is_none(), "the update is being merged already");
                let slot = self.info.slot.context("only an A/B phone merges an update")?;
                skyward_sys::kmsg("merging the update, as the update page asked")?;
                self.merger = Some(merge::Merger::start(slot, true)?);
            }
            Effect::Wlan(on) => wlan_worker(&self.wlan)?.send(if on { wlan::Command::On } else { wlan::Command::Off }),
            Effect::ScanWlan => {
                self.scanned_at = Instant::now();
                wlan_worker(&self.wlan)?.send(wlan::Command::Scan);
            }
            Effect::JoinWlan { ssid, name, security, password } => {
                wlan_worker(&self.wlan)?.send(wlan::Command::Join { ssid, name, security, password: password.0 })
            }
            Effect::LeaveWlan => wlan_worker(&self.wlan)?.send(wlan::Command::Leave),
            Effect::CheckUpdate => self.check_update()?,
            Effect::Download(release) => self.download(release, now)?,
            Effect::StopDownload => self.downloader.as_ref().context("there is no download to stop")?.stop(),
            Effect::CheckSkyward => {
                ensure!(self.skyward_checker.is_none(), "GitHub is being asked already");
                let probe = cfg.wlan.as_ref().context("the phone's WLAN is not brought up")?.probe.clone();
                let task = updates::Task::CheckSkyward { probe, codename: cfg.device.codename.clone() };
                self.skyward_checker = Some(updates::Worker::start(task, self.servers.clone())?);
            }
            Effect::UpdateSkyward(build) => {
                ensure!(self.skyward_updater.is_none(), "Skyward is being updated already");
                ensure!(self.installer.is_none(), "an installer is running already");
                skyward_sys::kmsg(&format!("updating Skyward to {}", build.name))?;
                let path = Path::new(skyward_sys::RUN).join(&build.name);
                let task = updates::Task::UpdateSkyward { build, codename: cfg.device.codename.clone(), path };
                self.skyward_updater = Some(updates::Worker::start(task, self.servers.clone())?);
            }
            Effect::StopSkywardUpdate => self.skyward_updater.as_ref().context("Skyward is not being updated")?.stop(),
            Effect::ExportLogs { recovery, kernel, storage } => {
                let into = match storage {
                    true => Path::new(skyward_crypto::data::SDCARD).join(STORAGE_LOGS),
                    false => PathBuf::from(TMP_LOGS),
                };
                match export_logs(recovery, kernel, &into, self.app.state.time_zone) {
                    Ok(folder) => self.app.report(&skyward_ui::fmt::exported(self.app.state.lang, &folder.display().to_string()), now),
                    Err(e) => self.app.report(&skyward_ui::fmt::export_failed(self.app.state.lang, &format!("{e:#}")), now),
                }
            }
            Effect::SetMode { path, mode } => {
                let outcome = skyward_files::set_mode(Path::new(&path), mode).map_err(|e| format!("{e:#}"));
                self.app.mode_set(mode, outcome, now);
            }
            Effect::OpenText(path) => {
                let read = skyward_files::read_up_to(Path::new(&path), skyward_ui::editor::MAX_BYTES).map_err(|e| format!("{e:#}"));
                self.app.text_opened(&self.fonts, path, read, now);
            }
            Effect::SaveText { path, text } => match skyward_files::replace(Path::new(&path), text.as_bytes()) {
                Ok(()) => {
                    self.app.text_saved(Ok(()), now);
                    let folder = self.app.state.files.path.clone();
                    list(&mut self.app, folder)?;
                }
                Err(e) => self.app.text_saved(Err(format!("{e:#}")), now),
            },
            Effect::Rename { path, name } => match skyward_files::rename(Path::new(&path), &name) {
                Ok(_) => {
                    self.app.renamed(Ok(()), now);
                    let folder = self.app.state.files.path.clone();
                    list(&mut self.app, folder)?;
                }
                Err(e) if e.is::<skyward_files::Taken>() => self.app.renamed(Err(RenameRefused::Taken), now),
                Err(e) => self.app.renamed(Err(RenameRefused::Failed(format!("{e:#}"))), now),
            },
            Effect::Mount(base) => {
                let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
                if let Err(e) = skyward_vab::mount_one(slot, &base) {
                    self.app.mount_failed(MountFailure { mounting: true, base, detail: format!("{e:#}") });
                }
                self.app.set_mounts(self.mounts());
            }
            Effect::Unmount(base) => {
                let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
                if let Err(e) = skyward_vab::unmount_one(slot, &base) {
                    self.app.mount_failed(MountFailure { mounting: false, base, detail: format!("{e:#}") });
                }
                self.app.set_mounts(self.mounts());
            }
            Effect::EnterRecovery => self.enter_recovery()?,
            Effect::FileTransfer(on) => self.file_transfer(on, now)?,
        }
        Ok(Vec::new())
    }

    fn systems(&mut self) -> Result<&mut systems::Worker> {
        self.systems_worker.as_mut().context("this phone holds no second system")
    }

    fn may_install(&mut self, now: u64) -> Result<bool> {
        ensure!(self.installer.is_none(), "an installer is running already");
        withhold(&mut self.transfer, &mut self.app, now)
    }

    fn reset_asked(&mut self, now: u64) -> Result<()> {
        ensure!(self.eraser.is_none(), "erasing is under way already");
        if !withhold(&mut self.transfer, &mut self.app, now)? {
            return Ok(());
        }
        self.eraser = Some(factory_reset(&mut self.app, self.request.keep_memtag_mode, true, None)?);
        self.erase_leaves = true;
        Ok(())
    }

    fn wipe(&mut self, chosen: Vec<Wipe>, now: u64) -> Result<()> {
        ensure!(self.eraser.is_none(), "erasing is under way already");
        if self.decryptor.is_some() {
            self.app.report(self.app.state.lang.pick(DATA_OPENING), now);
            return Ok(());
        }
        if !withhold(&mut self.transfer, &mut self.app, now)? {
            return Ok(());
        }
        self.erase_leaves = false;
        if chosen.contains(&Wipe::All) {
            self.eraser = Some(factory_reset(&mut self.app, false, false, None)?);
        } else if chosen.iter().any(|&w| w != Wipe::Cache) && self.app.state.data == Data::Erased {
            self.app.erase_started(false, false, Vec::new());
            self.app.erase_ended(Err(EraseStop::Erased), now);
        } else {
            let steps = chosen.into_iter().map(step).collect();
            self.eraser = Some(parts_wipe(&mut self.app, steps, &self.cfg.wipe.logs, false, None)?);
        }
        Ok(())
    }

    fn start_shell(&mut self, folder: &str, now: u64) -> Result<()> {
        ensure!(self.shell.is_none(), "a shell runs already");
        let (rows, cols) = self.app.term_size(&self.fonts);
        match pty::Shell::start(rows, cols, &self.cfg.device.codename, Path::new(folder)) {
            Ok(sh) => {
                self.shell = Some(sh);
                self.app.term_started(rows, cols);
            }
            Err(e) => {
                skyward_sys::kmsg(&format!("no shell for the terminal: {e:#}"))?;
                self.app.report(&format!("{e:#}"), now);
            }
        }
        Ok(())
    }

    fn shell_to(&mut self, folder: &str, now: u64) -> Result<()> {
        let sh = self.shell.as_mut().context("a folder went to a shell that has ended")?;
        let went = match sh.waiting() {
            Ok(true) => sh.change_to(folder),
            Ok(false) => {
                self.app.report(self.app.state.lang.pick(skyward_ui::fmt::TERMINAL_BUSY), now);
                Ok(())
            }
            Err(e) => Err(e),
        };
        if let Err(e) = went {
            self.app.report(&format!("{e:#}"), now);
        }
        Ok(())
    }

    fn cancel_install(&mut self, now: u64) -> Result<()> {
        match &self.installer {
            Some(i) => i.cancel()?,
            None => {
                ensure!(self.pending.take().is_some(), "no installer to cancel");
                skyward_sys::kmsg("the update asked for is given up, its page left")?;
                self.done_with_request()?;
                let cancelled = skyward_install::Event::Finished(skyward_install::Outcome::Cancelled);
                self.app.install(cancelled, now);
            }
        }
        Ok(())
    }

    fn paste(&mut self, op: FileOp, path: String, into: String, clash: Option<ClashChoice>, now: u64) -> Result<()> {
        ensure!(self.worker.is_none(), "work on files is under way already");
        let clash = match clash {
            Some(choice) => clash_rule(choice),
            None => match skyward_files::clashes(&[PathBuf::from(&path)], Path::new(&into)) {
                Ok(taken) if taken.is_empty() => skyward_files::Clash::Refuse,
                Ok(taken) => {
                    let names = taken.iter().map(|t| t.file_name().map_or_else(String::new, |n| n.to_string_lossy().into_owned())).collect();
                    self.app.file_clash(op, path, into, names);
                    return Ok(());
                }
                Err(e) => {
                    self.app.report(&format!("{e:#}"), now);
                    return Ok(());
                }
            },
        };
        let ownership = match ownership(Path::new(&into)) {
            Ok(ownership) => ownership,
            Err(e) => {
                self.app.report(&format!("{e:#}"), now);
                return Ok(());
            }
        };
        let (from, to) = (PathBuf::from(&path), PathBuf::from(&into));
        let (task, shown) = match op {
            FileOp::Copy => (filework::Task::Copy { from, into: to, clash, ownership }, FileTask::Copy { path, into }),
            FileOp::Move => (filework::Task::Move { from, into: to, clash, ownership }, FileTask::Move { path, into }),
        };
        self.worker = Some(filework::Worker::start(task)?);
        self.app.file_job_started(shown);
        Ok(())
    }

    fn relabel(&mut self, now: u64) -> Result<()> {
        ensure!(self.relabeler.is_none(), "relabeling is under way already");
        let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
        match relabel::rule_files(slot) {
            Ok(rules) => {
                self.relabeler = Some(filework::Worker::start(filework::Task::Relabel { rules })?);
                self.app.relabel_started();
            }
            Err(e) => match e.downcast_ref::<relabel::Unmounted>() {
                Some(relabel::Unmounted(base)) => self.app.report(&skyward_ui::fmt::mount_first(self.app.state.lang, base), now),
                None => self.app.report(&skyward_ui::fmt::rules_unread(self.app.state.lang, &format!("{e:#}")), now),
            },
        }
        Ok(())
    }

    fn filesystem(&mut self, name: &str, action: FsAction, now: u64) -> Result<()> {
        ensure!(self.eraser.is_none() && self.part_worker.is_none(), "work on partitions is under way already");
        if name == DATA_PARTITION && self.decryptor.is_some() {
            self.app.report(self.app.state.lang.pick(DATA_OPENING), now);
            return Ok(());
        }
        if name == DATA_PARTITION && !withhold(&mut self.transfer, &mut self.app, now)? {
            return Ok(());
        }
        let part = fs_part(name)?;
        let (kind, task) = match action {
            FsAction::Remake(filesystem) if name == DATA_PARTITION => {
                self.erase_leaves = false;
                self.eraser = Some(factory_reset(&mut self.app, false, false, Some(filesystem))?);
                return Ok(());
            }
            FsAction::Repair => (PartKind::Repair, partwork::Task::Repair(part)),
            FsAction::Remake(filesystem) => (PartKind::Remake(filesystem.clone()), partwork::Task::Remake(part, filesystem)),
        };
        skyward_sys::kmsg(&format!("{kind:?} the filesystem of {name}, as the filesystem page asked"))?;
        self.app.part_work_started(kind, false);
        self.part_worker = Some(partwork::Worker::start(task)?);
        Ok(())
    }

    fn back_up(&mut self, parts: [bool; 4], place: Place, compress: bool, verify: bool, now: u64) -> Result<()> {
        ensure!(self.backuper.is_none(), "a backup or restore is under way already");
        if self.installer.is_some() {
            self.app.report(self.app.state.lang.pick(INSTALL_UNFINISHED), now);
            return Ok(());
        }
        let into = backups_in(place);
        let there = into.ancestors().find(|p| p.exists()).context("the root is always there")?;
        let ownership = match ownership(there) {
            Ok(ownership) => ownership,
            Err(e) => {
                self.app.report(&format!("{e:#}"), now);
                return Ok(());
            }
        };
        let made = unix_now()?;
        let task = backup::BackupTask {
            parts,
            stamp: skyward_ui::fmt::stamp(made, self.app.state.time_zone),
            into,
            ownership,
            compress,
            verify,
            remount: (place == Place::UsbDrive).then(|| PathBuf::from(USB_DRIVE)),
            made,
            device: self.cfg.device.codename.clone(),
            system: self.info.os_version.clone(),
            efs: self.cfg.backup.efs.clone(),
            skipped: backup::data_skipped(&self.cfg.backup.data_skipped),
        };
        self.backuper = Some(backup::Worker::start(backup::Task::Backup(task))?);
        self.app.backup_started(false, place, verify);
        Ok(())
    }

    fn restore(&mut self, place: Place, name: &str, parts: [bool; 4], now: u64) -> Result<()> {
        ensure!(self.backuper.is_none(), "a backup or restore is under way already");
        if self.installer.is_some() {
            self.app.report(self.app.state.lang.pick(INSTALL_UNFINISHED), now);
            return Ok(());
        }
        let task = backup::RestoreTask {
            folder: backups_in(place).join(name),
            parts,
            device: self.cfg.device.codename.clone(),
            skipped: backup::data_skipped(&self.cfg.backup.data_skipped),
            detach: detach(self.programs.to_vec()),
        };
        self.backuper = Some(backup::Worker::start(backup::Task::Restore(task))?);
        self.app.backup_started(true, place, true);
        Ok(())
    }

    fn check_update(&mut self) -> Result<()> {
        ensure!(self.checker.is_none(), "the update server is being asked already");
        let server = self.cfg.update.as_ref().context("the phone's update server is not known")?.server.clone();
        let probe = self.cfg.wlan.as_ref().context("the phone's WLAN is not brought up")?.probe.clone();
        match installed(self.cfg, &self.info) {
            Ok(installed) => {
                self.checker = Some(updates::Worker::start(updates::Task::Check { server, probe, installed }, self.servers.clone())?)
            }
            Err(e) => self.app.update_checked(Err(Unanswered::Failed(format!("{e:#}"))), Have::Nothing),
        }
        Ok(())
    }

    fn download(&mut self, release: Release, now: u64) -> Result<()> {
        ensure!(self.downloader.is_none(), "the full package is being downloaded already");
        let server = self.cfg.update.as_ref().context("the phone's update server is not known")?.server.clone();
        let path = downloaded(&release);
        match installed(self.cfg, &self.info) {
            Ok(installed) => {
                let task = updates::Task::Download { server, installed, release, path };
                self.downloader = Some(updates::Worker::start(task, self.servers.clone())?);
            }
            Err(e) => {
                let have = have(&self.app, &release);
                self.app.download_ended(DownloadEnd::Failed(Failure::Other(format!("{e:#}"))), have, now);
            }
        }
        Ok(())
    }

    fn enter_recovery(&mut self) -> Result<()> {
        skyward_sys::usb::switch(skyward_sys::usb::resting(), USB_TIMEOUT)?;
        let slot = skyward_vab::current_slot()?.context("not an A/B phone")?;
        crate::prepare(self.cfg, slot)?;
        self.read_at = Instant::now();
        read_details(self.cfg, &mut self.info, &mut self.unread.details)?;
        self.fastboot = false;
        self.app.enter_recovery();
        if let Some(w) = self.systems_worker.as_mut() {
            w.send(systems::Task::Resume);
        }
        Ok(())
    }

    fn file_transfer(&mut self, on: bool, now: u64) -> Result<()> {
        let rests = !self.fastboot && !self.sideloading;
        match (on, self.transfer.take()) {
            (true, None) => match Transfer::start(self.cfg, &self.info, self.app.state.time_zone) {
                Ok(t) => {
                    self.transfer = Some(t);
                    if rests
                        && let Err(e) = skyward_sys::usb::switch(skyward_sys::usb::resting(), USB_TIMEOUT)
                    {
                        skyward_sys::kmsg(&format!("the port did not take file transfer: {e:#}"))?;
                        self.app.report(&format!("{e:#}"), now);
                    }
                }
                Err(e) => {
                    skyward_sys::kmsg(&format!("file transfer did not start: {e:#}"))?;
                    self.app.file_transfer_failed(false, &skyward_ui::fmt::transfer_failed(self.app.state.lang, &format!("{e:#}")), now);
                }
            },
            (false, Some(t)) => {
                if let Err(e) = t.stop(rests) {
                    skyward_sys::kmsg(&format!("file transfer did not stop cleanly: {e:#}"))?;
                    self.app.report(&skyward_ui::fmt::transfer_left_on(self.app.state.lang, &format!("{e:#}")), now);
                }
            }
            (on, t) => unreachable!("the switch turned file transfer {on} while it was {}", t.is_some()),
        }
        Ok(())
    }
}
