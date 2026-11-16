use crate::description::{Described, Kind};
use crate::phone::{Happens, Phone};
use crate::script::{Program, Socket};
use anyhow::{bail, ensure, Context, Result};
use skyward_config::Service;
use std::path::Path;

const KEYS: &str = "android.hardware.security.keymint.IKeyMintDevice/default";
const OLDER_KEYS: (&str, &str) = ("android.hardware.keymaster", "IKeymasterDevice");
const KEY_PACKAGES: [&str; 3] = ["android.hardware.security.keymint", "android.hardware.security.secureclock", "android.hardware.security.sharedsecret"];

const CREDENTIALS: (&str, &str) = ("android.hardware.gatekeeper", "IGatekeeper");
const BOOT: (&str, &str) = ("android.hardware.boot", "IBootControl");

const WEAVER: &str = "android.hardware.weaver.IWeaver/default";

const ELEMENT: (&str, &str) = ("android.hardware.secure_element", "ISecureElement");
const EMBEDDED: &str = "eSE";

const ELEMENT_SERVICE: &str = skyward_omapi::SERVICE;

const INSTANCE: &str = "default";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Through {
    ElementService,
    Own(&'static str),
}

const WEAVERS: [(&str, Through); 3] = [
    (".thales", Through::ElementService),
    (".nxp-qti", Through::ElementService),
    ("weaver-service", Through::Own("vendor.goodix.hardware.secure_element_aidl.IGdxEse")),
];

const LISTENERS: &str = "qseecomd";
const LISTENERS_READY: &str = "vendor.sys.listeners.registered";
const OPENERS: [&str; 2] = ["ssgtzd", "hlosminkdaemon"];

const HARDWARE_DIRECTORY: &str = "hwservicemanager";
const HARDWARE_DIRECTORY_READY: &str = "hwservicemanager.ready";

const VENDOR_LIBRARIES: [&str; 3] = ["/system/lib64", "/vendor/lib64", "/vendor/lib64/hw"];
const ODM_LIBRARIES: [&str; 4] = ["/system/lib64", "/vendor/lib64", "/vendor/lib64/hw", "/odm/lib64"];
const SYSTEM_LIBRARIES: [&str; 3] = ["/mnt/system/system/lib64/bootstrap", "/mnt/system/system/lib64", "/mnt/system_ext/lib64"];

const SHARED_LIBRARIES: [&str; 2] = ["/mnt/system/system/lib64", "/mnt/system/system/lib64/bootstrap"];

pub(crate) fn libraries_of(path: &str, bundle: Option<&Path>) -> Vec<String> {
    let (own, shared): (&[&str], &[&str]) = match path.split('/').nth(1) {
        Some("vendor") => (&VENDOR_LIBRARIES, &SHARED_LIBRARIES),
        Some("odm") => (&ODM_LIBRARIES, &SHARED_LIBRARIES),
        _ => (&SYSTEM_LIBRARIES, &[]),
    };
    let bundle = bundle.map(|dir| dir.to_string_lossy().into_owned());
    own.iter().map(|l| l.to_string()).chain(bundle).chain(shared.iter().map(|l| l.to_string())).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Started {
    pub service: Service,
    pub sockets: Vec<Socket>,
    pub environment: Vec<(String, String)>,
    pub anew: bool,
}

impl From<Service> for Started {
    fn from(service: Service) -> Started {
        Started { service, sockets: Vec::new(), environment: Vec::new(), anew: false }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    Start(Box<Started>),
    ServeElements,
}

enum Ready {
    Nothing,
    Property(&'static str),
    Service(String),
    HardwareService(String),
}

pub fn steps(phone: &Phone) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    let start = |out: &mut Vec<Step>, started: Started| out.push(Step::Start(Box::new(started)));
    if let Some(listeners) = named(phone, LISTENERS)? {
        start(&mut out, started(phone, listeners, Ready::Property(LISTENERS_READY), Vec::new()));
    }
    for opener in OPENERS {
        if let Some(program) = named(phone, opener)? {
            start(&mut out, started(phone, program, Ready::Nothing, Vec::new()));
        }
    }

    let (keys_name, keys_kind) = if described(phone, Kind::Aidl, KEYS).is_some() {
        (KEYS.to_string(), Kind::Aidl)
    } else if let Some(older) = offered(phone, OLDER_KEYS) {
        older
    } else {
        bail!("the phone describes no {KEYS}");
    };
    let (credentials, credentials_kind) = offered(phone, CREDENTIALS).context("the phone describes no credential service")?;
    let (boot, boot_kind) = offered(phone, BOOT).context("the phone describes no boot control")?;
    let start_hardware_directory = |out: &mut Vec<Step>| -> Result<()> {
        if let Some(directory) = phone.programs.iter().find(|p| p.name == HARDWARE_DIRECTORY && phone.carries(p)) {
            start(out, started(phone, directory, Ready::Property(HARDWARE_DIRECTORY_READY), Vec::new()));
        } else if Path::new("/mnt/system/system/bin/hwservicemanager").is_file() {
            let service = Service {
                path: "/mnt/system/system/bin/hwservicemanager".to_string(),
                args: Vec::new(),
                libraries: libraries_of("/system/bin/hwservicemanager", phone.bundle.as_deref()),
                oneshot: false,
                ready_property: Some(HARDWARE_DIRECTORY_READY.to_string()),
                ready_service: None,
                registers: Vec::new(),
                ready_hardware_service: None,
                when: None,
                user: Some("system".to_string()),
                groups: vec!["system".to_string(), "readproc".to_string()],
                capabilities: Vec::new(),
            };
            start(out, Started::from(service));
        } else {
            bail!("the phone has hardware services of the older kind, and no directory for them among the programs its scripts name");
        }
        Ok(())
    };
    if keys_kind == Kind::Hidl {
        start_hardware_directory(&mut out)?;
    }
    let ready = |name: String, kind: Kind| match kind {
        Kind::Aidl => Ready::Service(name),
        Kind::Hidl => Ready::HardwareService(name),
    };
    let keys = behind(phone, "the key service", &keys_name, |file| {
        (file.contains("keymint") || (file.contains("keymaster") && file != "keymasterd")) && !file.contains("strongbox")
    })?;
    let with_keys = |d: &Described| d.instance == INSTANCE && KEY_PACKAGES.contains(&d.package.as_str());
    let registers = match keys_kind {
        Kind::Aidl => besides(phone, keys, &keys_name, with_keys),
        Kind::Hidl => Vec::new(),
    };
    start(&mut out, started(phone, keys, ready(keys_name, keys_kind), registers));

    if keys_kind != Kind::Hidl && (credentials_kind == Kind::Hidl || boot_kind == Kind::Hidl) {
        start_hardware_directory(&mut out)?;
    }
    let alone = |_: &Described| false;
    let program = behind(phone, "the credential service", &credentials, |file| file.contains("gatekeeper"))?;
    let registers = besides(phone, program, &credentials, alone);
    start(&mut out, started(phone, program, ready(credentials, credentials_kind), registers));

    if described(phone, Kind::Aidl, WEAVER).is_some() {
        out.extend(weaver(phone)?);
    }

    let program = behind(phone, "the boot control", &boot, |file| file.starts_with(BOOT.0))?;
    let registers = besides(phone, program, &boot, alone);
    start(&mut out, started(phone, program, ready(boot, boot_kind), registers));
    Ok(out)
}

fn own_description<'a>(phone: &'a Phone, program: &Program, name: &str) -> Option<&'a str> {
    let told = described(phone, Kind::Aidl, name)?;
    (!program.script.is_empty() && stem(&program.script) == stem(&told.file)).then_some(told.file.as_str())
}

fn besides(phone: &Phone, program: &Program, name: &str, also: impl Fn(&Described) -> bool) -> Vec<String> {
    let own = own_description(phone, program, name);
    let mut out = Vec::new();
    for service in phone.services.iter().filter(|s| s.described.kind == Kind::Aidl) {
        let other = service.described.name();
        if (own == Some(service.file.as_str()) || also(&service.described)) && other != name && !out.contains(&other) {
            out.push(other);
        }
    }
    out
}

pub fn programs(steps: &[Step]) -> Vec<&Started> {
    steps
        .iter()
        .filter_map(|step| match step {
            Step::Start(started) => Some(started.as_ref()),
            Step::ServeElements => None,
        })
        .collect()
}

fn weaver(phone: &Phone) -> Result<Vec<Step>> {
    let mut out = Vec::new();
    let program = behind(phone, "the weaver", WEAVER, |file| file.contains("weaver-service"))?;
    let file = file_name(program);
    let (_, through) = WEAVERS.iter().find(|(ending, _)| file.ends_with(ending)).with_context(|| format!("{} is a weaver of a make the recovery does not know", program.path))?;
    match through {
        Through::Own(interface) => {
            let own = phone
                .services
                .iter()
                .find(|s| s.described.kind == Kind::Aidl && format!("{}.{}", s.described.package, s.described.interface) == *interface)
                .with_context(|| format!("{} goes through {interface}, which the phone does not describe", program.path))?;
            out.push(Step::Start(Box::new(element(phone, &own.described.name())?)));
        }
        Through::ElementService => {
            ensure!(described(phone, Kind::Aidl, ELEMENT_SERVICE).is_some(), "{} goes through {ELEMENT_SERVICE}, which the phone does not describe", program.path);
            let embedded: Vec<String> = phone
                .services
                .iter()
                .map(|s| &s.described)
                .filter(|d| d.kind == Kind::Aidl && (d.package.as_str(), d.interface.as_str()) == ELEMENT && d.instance.starts_with(EMBEDDED))
                .map(Described::name)
                .collect();
            ensure!(!embedded.is_empty(), "{} goes through a secure element built into the phone, and the phone describes the service of none", program.path);
            for name in embedded {
                let started = element(phone, &name)?;
                if !out.iter().any(|step| matches!(step, Step::Start(s) if s.service.path == started.service.path)) {
                    out.push(Step::Start(Box::new(started)));
                }
            }
            out.push(Step::ServeElements);
        }
    }
    let mut weaver = started(phone, program, Ready::Service(WEAVER.into()), besides(phone, program, WEAVER, |_| false));
    weaver.anew = *through == Through::ElementService;
    out.push(Step::Start(Box::new(weaver)));
    Ok(out)
}

fn element(phone: &Phone, name: &str) -> Result<Started> {
    let program = behind(phone, "a secure element's service", name, |file| file.contains("secure_element"))?;
    let told = described(phone, Kind::Aidl, name).expect("the service was found among those described");
    let of_an_element = |d: &Described| (d.package.as_str(), d.interface.as_str()) == ELEMENT;
    let own = match of_an_element(&told.described) {
        true => name.to_string(),
        false => {
            let beside = own_description(phone, program, name).and_then(|file| phone.services.iter().find(|s| s.file == file && s.described.kind == Kind::Aidl && of_an_element(&s.described)));
            beside.with_context(|| format!("{} registers {name}, and its description tells of the service of no secure element beside it", program.path))?.described.name()
        }
    };
    let registers = besides(phone, program, &own, |_| false);
    Ok(started(phone, program, Ready::Service(own), registers))
}

fn described<'a>(phone: &'a Phone, kind: Kind, name: &str) -> Option<&'a crate::phone::Service> {
    phone.services.iter().find(|s| s.described.kind == kind && s.described.name() == name)
}

fn offered(phone: &Phone, (package, interface): (&str, &str)) -> Option<(String, Kind)> {
    let of = |kind: Kind| phone.services.iter().map(|s| &s.described).filter(move |d| d.kind == kind && d.package == package && d.interface == interface && d.instance == INSTANCE);
    if let Some(newer) = of(Kind::Aidl).next() {
        return Some((newer.name(), Kind::Aidl));
    }
    let version = |d: &&Described| -> Vec<u32> { d.version.iter().flat_map(|v| v.split('.')).filter_map(|n| n.parse().ok()).collect() };
    of(Kind::Hidl).max_by_key(version).map(|older| (older.name(), Kind::Hidl))
}

fn file_name(program: &Program) -> &str {
    program.path.rsplit('/').next().unwrap_or(&program.path)
}

fn stem(path: &str) -> &str {
    let file = path.rsplit('/').next().unwrap_or(path);
    file.rsplit_once('.').map_or(file, |(stem, _)| stem)
}

fn named<'a>(phone: &'a Phone, file: &str) -> Result<Option<&'a Program>> {
    one(phone, file, phone.programs.iter().filter(|p| file_name(p) == file).collect())
}

fn on_demand(program: &Program) -> bool {
    program.disabled && !program.interfaces.is_empty()
}

fn behind<'a>(phone: &'a Phone, what: &str, name: &str, matches: impl Fn(&str) -> bool) -> Result<&'a Program> {
    let saying: Vec<&Program> = phone.programs.iter().filter(|p| p.interfaces.iter().any(|i| i == name)).collect();
    let may: Vec<&Program> = phone.programs.iter().filter(|p| !on_demand(p)).collect();
    let described_with: Vec<&Program> = match phone.services.iter().find(|s| s.described.name() == name) {
        Some(service) => may.iter().copied().filter(|p| !p.script.is_empty() && stem(&p.script) == stem(&service.file)).collect(),
        None => Vec::new(),
    };
    let matching: Vec<&Program> = may.iter().copied().filter(|p| matches(file_name(p))).collect();
    for candidates in [saying, described_with, matching] {
        if let Some(program) = one(phone, what, candidates)? {
            return Ok(program);
        }
    }
    bail!("no script of the phone's runs {what}, which would register {name}")
}

fn one<'a>(phone: &Phone, what: &str, candidates: Vec<&'a Program>) -> Result<Option<&'a Program>> {
    for wanted in [Happens::Yes, Happens::Maybe] {
        let running: Vec<&Program> = candidates.iter().copied().filter(|p| phone.runs(p) == wanted).collect();
        match running[..] {
            [] => continue,
            [program] => return Ok(Some(program)),
            _ => bail!("the phone runs more than one program for {what}: {}", running.iter().map(|p| p.path.as_str()).collect::<Vec<_>>().join(", ")),
        }
    }
    Ok(None)
}

fn started(phone: &Phone, program: &Program, ready: Ready, registers: Vec<String>) -> Started {
    let (ready_property, ready_service, ready_hardware_service) = match ready {
        Ready::Nothing => (None, None, None),
        Ready::Property(name) => (Some(name.to_string()), None, None),
        Ready::Service(name) => (None, Some(name), None),
        Ready::HardwareService(name) => (None, None, Some(name)),
    };
    let service = Service {
        path: phone.started_from(&program.path),
        args: program.args.clone(),
        libraries: libraries_of(&program.path, phone.bundle.as_deref()),
        oneshot: false,
        ready_property,
        ready_service,
        registers,
        ready_hardware_service,
        when: None,
        user: program.user.clone(),
        groups: program.groups.clone(),
        capabilities: program.capabilities.clone().unwrap_or_default(),
    };
    Started { service, sockets: program.sockets.clone(), environment: program.environment.clone(), anew: false }
}
