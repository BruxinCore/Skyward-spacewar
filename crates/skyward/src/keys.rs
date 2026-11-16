use anyhow::{Context, Result};
use skyward_config::Service;
use skyward_programs::naming::{self, Does};
use skyward_programs::phone::{self, Partition, Phone, PARTITIONS};
use skyward_programs::secure::{self, Step};
use std::path::Path;

pub struct Found {
    pub described: Vec<String>,
    pub steps: Result<Vec<Step>>,
}

pub fn found(slot: skyward_vab::Slot, libraries: Option<&Path>) -> Result<Found> {
    let mut phone = read(slot, libraries)?;
    let unnamed = naming::steps(&phone)?;
    if !unnamed.is_empty() {
        name(&unnamed)?;
        phone = read(slot, libraries)?;
    }
    Ok(Found { described: phone.told_of(), steps: secure::steps(&phone) })
}

fn read(slot: skyward_vab::Slot, libraries: Option<&Path>) -> Result<Phone> {
    let properties = skyward_info::installed_props(slot)?;
    let mounted = skyward_vab::mounted_partitions(slot)?;
    let mut partitions = Vec::new();
    for name in PARTITIONS {
        let named = format!("{name}{}", slot.suffix());
        let mount_point = match mounted.iter().find(|m| m.partition == named) {
            Some(partition) => partition.mount_point.clone(),
            None => {
                let merged = [Path::new("/mnt/system/system").join(name), Path::new("/mnt/system").join(name)]
                    .into_iter()
                    .find(|p| p.is_dir())
                    .with_context(|| format!("{named} is not mounted"))?;
                let link = Path::new("/mnt").join(name);
                if !link.exists() {
                    let _ = std::os::unix::fs::symlink(&merged, &link);
                }
                merged
            }
        };
        partitions.push(Partition::mounted(name, mount_point));
    }
    Phone::read(partitions, properties, libraries)
}

fn name(does: &[Does]) -> Result<()> {
    for one in does {
        match one {
            Does::Run(program) => {
                let status = crate::services::once(program)?;
                skyward_sys::kmsg(&format!("ran {} {}, which ended: {status}", program.service.path, program.service.args.join(" ")))?;
            }
            Does::Set { name, value } => match phone::expand(value, skyward_sys::prop) {
                Some(value) => {
                    skyward_sys::set_prop(name, &value)?;
                    skyward_sys::kmsg(&format!("{name} is {value}, as the phone's init sets it"))?;
                }
                None => skyward_sys::kmsg(&format!("{name} stays unset, as the phone's init leaves it: nothing has set what {value} names"))?,
            },
        }
    }
    Ok(())
}

pub fn programs(steps: &[Step]) -> Vec<Service> {
    secure::programs(steps).into_iter().map(|started| started.service.clone()).collect()
}

pub fn serves_elements(steps: &[Step]) -> bool {
    steps.contains(&Step::ServeElements)
}

pub fn start(steps: &[Step]) -> Result<()> {
    for step in steps {
        match step {
            Step::Start(program) => crate::services::start(std::slice::from_ref(program.as_ref()), None)?,
            Step::ServeElements => {
                let readers = skyward_omapi::start()?;
                skyward_sys::kmsg(&format!("{} is served, with a reader for each of {} secure elements: {}", skyward_omapi::SERVICE, readers.len(), readers.join(", ")))?;
            }
        }
    }
    Ok(())
}

pub fn describe(steps: &[Step]) -> Vec<String> {
    let mut lines = Vec::new();
    for step in steps {
        let Step::Start(program) = step else {
            lines.push(format!("serve {}", skyward_omapi::SERVICE));
            continue;
        };
        let s = &program.service;
        let mut line = format!("start {}", s.path);
        for word in &s.args {
            line.push_str(&format!(" {word}"));
        }
        line.push_str(&format!("\n  as {} in {}", s.user.as_deref().unwrap_or("root"), if s.groups.is_empty() { "root".to_string() } else { s.groups.join(" ") }));
        if !s.capabilities.is_empty() {
            line.push_str(&format!(", keeping {}", s.capabilities.join(" ")));
        }
        for socket in &program.sockets {
            let owner = format!("{}:{}", socket.user.as_deref().unwrap_or("root"), socket.group.as_deref().unwrap_or("root"));
            line.push_str(&format!("\n  with the socket {} ({:?}, {:o}, {owner})", socket.name, socket.kind, socket.mode));
        }
        for (name, value) in &program.environment {
            line.push_str(&format!("\n  with {name}={value}"));
        }
        for ready in s.ready_property.iter().chain(&s.ready_service).chain(&s.ready_hardware_service) {
            line.push_str(&format!("\n  ready with {ready}"));
        }
        for name in &s.registers {
            line.push_str(&format!("\n  registering {name} as well"));
        }
        if program.anew {
            line.push_str("\n  started anew where an earlier recovery service left it running");
        }
        lines.push(line);
    }
    lines
}
