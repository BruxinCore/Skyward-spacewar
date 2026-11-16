use anyhow::{ensure, Context, Result};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const BUNDLE_DIRS: [&str; 4] = ["/mnt/system_ext/apex", "/mnt/system/system/apex", "/mnt/product/apex", "/vendor/apex"];

const BUNDLE_IMAGE: &str = "apex_payload.img";

const BUNDLE_MOUNT: &str = "/apex";
const BUNDLE_LIBS: &str = "lib64";

const VENDOR_INTERFACE: &str = "ro.vndk.version";

const RECOVERY_DESCRIPTIONS: &str = "/system/etc/vintf/manifest";
const PHONE_DESCRIPTION: &str = "phone.xml";

const DIRECTORY_SERVICE: &str = "servicemanager";
const DIRECTORY_PROGRAM: &str = "/system/bin/servicemanager";

const SERVICE_PIDS: &str = "init.svc_debug_pid.";

const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

const STOP_TIMEOUT: Duration = Duration::from_secs(5);

const EARLY_LOADER: &str = "/system/bin/bootstrap/linker64";
const RUNTIME_BUNDLE: &str = "com.android.runtime";
const BUNDLE_LOADER: &str = "bin/linker64";

const SERVICE_LABELS: &str = "/mnt/system/system/etc/selinux/plat_hwservice_contexts";
const RECOVERY_LABELS: &str = "/system/etc/selinux";

const HARDWARE_DRIVER: &str = "/dev/hwbinder";
const HARDWARE_DRIVER_NODE: &str = "/dev/binderfs/hwbinder";

const HARDWARE_DIRECTORY: &str = "android.hidl.manager@1.2::IServiceManager/default";

const DIRECTORY_OFF: &str = "hwservicemanager.disabled";

const RECOVERY_MANIFEST: &str = "/system/etc/vintf/manifest.xml";

pub fn prepare_hardware_directory(wanted: &[String]) -> Result<()> {
    skyward_binder::driver::ensure_binderfs();
    let loader = Path::new(EARLY_LOADER);
    let from = bundle(RUNTIME_BUNDLE)?
        .map(|at| at.join(BUNDLE_LOADER))
        .filter(|p| p.is_file())
        .with_context(|| format!("the installed system carries no {RUNTIME_BUNDLE}, which holds the loader its own programs ask for"))?;
    let dir = loader.parent().expect("the loader is in a directory");
    fs::create_dir_all(dir).with_context(|| format!("make {}", dir.display()))?;
    fs::set_permissions(dir, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .with_context(|| format!("open {} to every user", dir.display()))?;
    if loader.exists() {
        fs::remove_file(loader).with_context(|| format!("take away {}", loader.display()))?;
    }
    std::os::unix::fs::symlink(&from, loader).with_context(|| format!("point {} at {}", loader.display(), from.display()))?;
    let driver = Path::new(HARDWARE_DRIVER);
    if !driver.exists() {
        std::os::unix::fs::symlink(HARDWARE_DRIVER_NODE, driver)
            .with_context(|| format!("point {HARDWARE_DRIVER} at {HARDWARE_DRIVER_NODE}"))?;
    }
    fs::set_permissions(driver, std::os::unix::fs::PermissionsExt::from_mode(0o666))
        .with_context(|| format!("open {HARDWARE_DRIVER} to every program"))?;
    let from = Path::new(SERVICE_LABELS);
    let into = Path::new(RECOVERY_LABELS).join(from.file_name().expect("a named file"));
    fs::create_dir_all(RECOVERY_LABELS).with_context(|| format!("make {RECOVERY_LABELS}"))?;
    fs::set_permissions(RECOVERY_LABELS, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .with_context(|| format!("open {RECOVERY_LABELS} to every user"))?;
    fs::copy(from, &into).with_context(|| format!("copy {} to {}", from.display(), into.display()))?;
    fs::set_permissions(&into, std::os::unix::fs::PermissionsExt::from_mode(0o644))
        .with_context(|| format!("let every user read {}", into.display()))?;
    let manifest_dir = Path::new(RECOVERY_MANIFEST).parent().expect("a named file");
    fs::create_dir_all(manifest_dir).with_context(|| format!("make {}", manifest_dir.display()))?;
    fs::set_permissions(manifest_dir, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .with_context(|| format!("open {} to every user", manifest_dir.display()))?;
    fs::write(RECOVERY_MANIFEST, describe_hardware(wanted)?).with_context(|| format!("write {RECOVERY_MANIFEST}"))?;
    fs::set_permissions(RECOVERY_MANIFEST, std::os::unix::fs::PermissionsExt::from_mode(0o644))
        .with_context(|| format!("let every user read {RECOVERY_MANIFEST}"))?;
    skyward_sys::set_prop(DIRECTORY_OFF, "false")?;
    Ok(())
}

fn describe_hardware(wanted: &[String]) -> Result<String> {
    let mut out = String::from("<manifest version=\"1.0\" type=\"framework\">\n");
    for name in std::iter::once(HARDWARE_DIRECTORY).chain(wanted.iter().map(String::as_str)) {
        let (package, rest) = name
            .split_once('@')
            .with_context(|| format!("{name} does not read as a hardware service's name"))?;
        out.push_str("    <hal format=\"hidl\">\n");
        out.push_str(&format!("        <name>{package}</name>\n"));
        out.push_str("        <transport>hwbinder</transport>\n");
        out.push_str(&format!("        <fqname>@{rest}</fqname>\n"));
        out.push_str("    </hal>\n");
    }
    out.push_str("</manifest>\n");
    Ok(out)
}

pub fn libraries() -> Result<Option<PathBuf>> {
    let Some(version) = skyward_sys::prop(VENDOR_INTERFACE) else { return Ok(None) };
    Ok(bundle(&format!("com.android.vndk.v{version}"))?.map(|at| at.join(BUNDLE_LIBS)))
}

fn bundle(name: &str) -> Result<Option<PathBuf>> {
    let at = Path::new(BUNDLE_MOUNT).join(name);
    if skyward_block::mount::is_mount_point(&at)? {
        return Ok(Some(at));
    }
    let Some(file) = BUNDLE_DIRS.iter().map(|d| Path::new(d).join(format!("{name}.apex"))).find(|p| p.is_file()) else {
        return Ok(None);
    };
    let (offset, length) = image(&file)?;
    let device = skyward_block::loopdev::attach(&file, offset, length)?;
    fs::create_dir_all(&at).with_context(|| format!("make {}", at.display()))?;
    if let Some(parent) = at.parent() {
        let _ = fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    }
    skyward_block::mount::mount_ro(&device, &at)?;
    Ok(Some(at))
}

fn image(file: &Path) -> Result<(u64, u64)> {
    let f = fs::File::open(file).with_context(|| format!("open {}", file.display()))?;
    let mut bundle = zip::ZipArchive::new(f).with_context(|| format!("read what is in {}", file.display()))?;
    let entry = bundle.by_name(BUNDLE_IMAGE).with_context(|| format!("{} holds no {BUNDLE_IMAGE}", file.display()))?;
    if entry.compression() != zip::CompressionMethod::Stored {
        anyhow::bail!("{} keeps its {BUNDLE_IMAGE} packed, which cannot be opened where it lies", file.display());
    }
    let start = entry.data_start().with_context(|| format!("{BUNDLE_IMAGE} in {} says where it is not", file.display()))?;
    Ok((start, entry.size()))
}

pub fn publish_descriptions(wanted: &[String]) -> Result<()> {
    let into = Path::new(RECOVERY_DESCRIPTIONS);
    fs::create_dir_all(into).with_context(|| format!("make {}", into.display()))?;
    if let Some(parent) = into.parent() {
        let _ = fs::set_permissions(parent, std::os::unix::fs::PermissionsExt::from_mode(0o755));
    }
    fs::set_permissions(into, std::os::unix::fs::PermissionsExt::from_mode(0o755))
        .with_context(|| format!("open {} to every user", into.display()))?;
    let to = into.join(PHONE_DESCRIPTION);
    let mut shipped = vec![PathBuf::from(RECOVERY_MANIFEST)];
    for entry in fs::read_dir(into).with_context(|| format!("read {}", into.display()))? {
        shipped.push(entry.with_context(|| format!("read {}", into.display()))?.path());
    }
    let mut told = Vec::new();
    for path in shipped.iter().filter(|p| **p != to && p.extension().is_some_and(|e| e == "xml") && p.is_file()) {
        let text = fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
        let services = skyward_programs::description::read(&text).with_context(|| format!("read {}", path.display()))?;
        told.extend(services.iter().map(skyward_programs::description::Described::name));
    }
    let untold: Vec<String> = wanted.iter().filter(|name| !told.contains(name)).cloned().collect();
    fs::write(&to, skyward_programs::description::offering(&untold)?).with_context(|| format!("write {}", to.display()))?;
    fs::set_permissions(&to, std::os::unix::fs::PermissionsExt::from_mode(0o644)).with_context(|| format!("let every user read {}", to.display()))?;
    let rewritten = say_framework(into)?;
    let binder = skyward_binder::driver::Binder::open()?;
    let mut unknown = false;
    for name in wanted {
        if !skyward_binder::service::declared(&binder, name).with_context(|| format!("ask the service directory whether it knows {name} described"))? {
            unknown = true;
            break;
        }
    }
    if unknown || rewritten > 0 {
        let registered = registered()?;
        let was = crate::services::running(DIRECTORY_PROGRAM);
        skyward_sys::set_prop("ctl.restart", DIRECTORY_SERVICE).context("have the service directory start again")?;
        answering_again(was)?;
        register_again(&registered)?;
    }
    Ok(())
}

fn registered() -> Result<Vec<(String, String)>> {
    let Some(directory) = crate::services::running(DIRECTORY_PROGRAM) else { return Ok(Vec::new()) };
    let binder = skyward_binder::driver::Binder::open()?;
    let started = skyward_sys::props(SERVICE_PIDS);
    let mut out = Vec::new();
    for name in skyward_binder::service::names(&binder)? {
        let Some(service) = skyward_binder::service::find(&binder, &name)? else { continue };
        let pid = skyward_binder::service::pid(&service).with_context(|| format!("ask which process serves {name}"))?;
        if pid == directory {
            continue;
        }
        let (prop, _) = started
            .iter()
            .find(|(_, value)| value.parse() == Ok(pid))
            .with_context(|| format!("{name} is served by process {pid}, which init does not run, so nothing would register it again"))?;
        out.push((name, prop[SERVICE_PIDS.len()..].to_string()));
    }
    Ok(out)
}

fn register_again(registered: &[(String, String)]) -> Result<()> {
    let services: BTreeSet<&str> = registered.iter().map(|(_, service)| service.as_str()).collect();
    for service in &services {
        skyward_sys::set_prop("ctl.stop", service).with_context(|| format!("have init stop {service}"))?;
    }
    for service in &services {
        skyward_sys::wait_prop(&format!("init.svc.{service}"), "stopped", STOP_TIMEOUT)?;
        skyward_sys::set_prop("ctl.start", service).with_context(|| format!("have init start {service} again"))?;
    }
    let binder = skyward_binder::driver::Binder::open()?;
    let until = Instant::now() + REGISTER_TIMEOUT;
    for (name, service) in registered {
        while skyward_binder::service::find(&binder, name)?.is_none() {
            ensure!(Instant::now() < until, "{name} has not registered again since init started {service} again");
            std::thread::sleep(Duration::from_millis(50));
        }
        skyward_sys::kmsg(&format!("{name} registered again from {service}"))?;
    }
    Ok(())
}

fn as_framework(text: &str) -> String {
    text.replace("type=\"device\"", "type=\"framework\"").replace("type='device'", "type='framework'")
}

fn say_framework(dir: &Path) -> Result<usize> {
    let mut rewritten = 0;
    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let path = entry.with_context(|| format!("read {}", dir.display()))?.path();
        if path.extension().is_none_or(|e| e != "xml") {
            continue;
        }
        let text = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        let said = as_framework(&text);
        if said != text {
            fs::write(&path, said).with_context(|| format!("rewrite {}", path.display()))?;
            rewritten += 1;
        }
    }
    Ok(rewritten)
}

fn answering_again(was: Option<u32>) -> Result<()> {
    let until = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        let now = crate::services::running(DIRECTORY_PROGRAM);
        if now.is_some() && now != was && crate::services::reachable() {
            return Ok(());
        }
        if std::time::Instant::now() >= until {
            anyhow::bail!("the service directory did not come back");
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
}
