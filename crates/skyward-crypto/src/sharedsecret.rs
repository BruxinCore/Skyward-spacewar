#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use anyhow::{Context, Result};
use skyward_binder::hidl::{Answer, Message};
use skyward_binder::parcel::{Parcel, Reader};
use skyward_binder::status;

pub const INTERFACE: &str = "android.hardware.security.sharedsecret.ISharedSecret";
const HARDWARE_INTERFACE: &str = "android.hardware.keymaster@4.0::IKeymasterDevice";

const GET_PARAMETERS: u32 = 1;
const COMPUTE: u32 = 2;

const HARDWARE_GET_PARAMETERS: u32 = 2;
const HARDWARE_COMPUTE: u32 = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Parameters {
    pub seed: Vec<u8>,
    pub nonce: Vec<u8>,
}

fn parameters(read: &mut Reader) -> Result<Parameters> {
    status::ok(read).context("ask what the secure world brings to the agreement")?;
    let end = read.structure().context("read what the secure world brings")?;
    let seed = read.blob().context("read the seed the secure world keeps")?;
    let nonce = read.blob().context("read the number the secure world drew")?;
    read.go(end)?;
    Ok(Parameters { seed, nonce })
}

fn ask_compute(all: &[Parameters]) -> Parcel {
    let mut call = Parcel::call(INTERFACE);
    call.i32(all.len() as i32);
    for p in all {
        call.i32(1);
        let at = call.here();
        call.i32(0);
        call.blob(&p.seed);
        call.blob(&p.nonce);
        let size = (call.here() - at) as i32;
        call.fill(at, size);
    }
    call
}

fn agreed(read: &mut Reader) -> Result<Vec<u8>> {
    status::ok(read).context("have the secure world work out the key it signs its notes with")?;
    read.blob().context("read what the secure world answered about the key")
}

fn hardware_parameters(read: &mut Answer) -> Result<Parameters> {
    read.status().context("ask what the secure world brings to the agreement")?;
    let err = read.i32().context("ask what the secure world brings to the agreement")?;
    anyhow::ensure!(err == 0, "the key service refused the agreement with error {err}");
    let raw = read.pointed().context("read what the secure world brings")?;
    anyhow::ensure!(raw.len() >= 48, "the key service answered {} bytes of sharing parameters", raw.len());
    let seed_len = u32::from_le_bytes(raw[8..12].try_into().expect("four bytes")) as usize;
    let seed_raw = read.pointed_slice().context("read the seed the secure world keeps")?;
    let seed = seed_raw[..seed_len.min(seed_raw.len())].to_vec();
    let nonce = raw[16..48].to_vec();
    Ok(Parameters { seed, nonce })
}

fn ask_hardware_compute(all: &[Parameters]) -> Message {
    let mut call = Message::call(HARDWARE_INTERFACE);
    let mut raw = vec![0u8; all.len() * 48];
    let mut seeds = Vec::with_capacity(all.len());
    for (i, p) in all.iter().enumerate() {
        let entry = &mut raw[i * 48..(i + 1) * 48];
        let seed_at = call.keep(p.seed.clone());
        entry[0..8].copy_from_slice(&seed_at.to_le_bytes());
        entry[8..12].copy_from_slice(&(p.seed.len() as u32).to_le_bytes());
        let nonce_len = p.nonce.len().min(32);
        entry[16..16 + nonce_len].copy_from_slice(&p.nonce[..nonce_len]);
        seeds.push((i * 48, seed_at, p.seed.len()));
    }
    let parent = call.framed_vec(raw, all.len());
    for (offset, at, len) in seeds {
        call.address_at(at, len, Some((parent, offset)));
    }
    call
}

fn hardware_agreed(read: &mut Answer) -> Result<Vec<u8>> {
    read.status().context("have the secure world work out the key it signs its notes with")?;
    let err = read.i32().context("have the secure world work out the key it signs its notes with")?;
    let check = read.vec_bytes().context("read what the secure world answered about the key")?.to_vec();
    anyhow::ensure!(err == 0, "the key service refused to work out the key with error {err}");
    Ok(check)
}

#[cfg(target_os = "android")]
pub struct SharedSecret<'a> {
    device: skyward_binder::driver::Handle<'a>,
}

#[cfg(target_os = "android")]
impl<'a> SharedSecret<'a> {
    pub fn open(binder: &'a skyward_binder::driver::Binder, name: &str) -> Result<SharedSecret<'a>> {
        let device = skyward_binder::service::find(binder, name)?
            .with_context(|| format!("{name} is not running; the phone's key service has to be started first"))?;
        Ok(SharedSecret { device })
    }

    pub fn parameters(&self) -> Result<Parameters> {
        let reply = self
            .device
            .transact(GET_PARAMETERS, &Parcel::call(INTERFACE))
            .context("ask what the secure world brings to the agreement")?;
        parameters(&mut reply.read())
    }

    pub fn compute(&self, all: &[Parameters]) -> Result<Vec<u8>> {
        let reply = self
            .device
            .transact(COMPUTE, &ask_compute(all))
            .context("have the secure world work out the key it signs its notes with")?;
        agreed(&mut reply.read())
    }
}

#[cfg(target_os = "android")]
pub fn agree(binder: &skyward_binder::driver::Binder, names: &[String]) -> Result<usize> {
    let parts: Vec<SharedSecret> = names.iter().map(|n| SharedSecret::open(binder, n)).collect::<Result<_>>()?;
    let mut all: Vec<Parameters> = parts.iter().map(|p| p.parameters()).collect::<Result<_>>()?;
    all.sort();
    let mut agreed = None;
    for (part, name) in parts.iter().zip(names) {
        let said = part.compute(&all).with_context(|| format!("have {name} work out the key it signs its notes with"))?;
        anyhow::ensure!(
            agreed.get_or_insert_with(|| said.clone()) == &said,
            "{name} worked out another key than the rest of the secure world"
        );
    }
    Ok(parts.len())
}

#[cfg(target_os = "android")]
pub fn agree_hardware(hardware: &skyward_binder::driver::Binder, names: &[String]) -> Result<usize> {
    let mut parts = Vec::new();
    for name in names {
        let (iface, instance) = name.split_once('/').unwrap_or((name.as_str(), "default"));
        let handle = skyward_binder::hidl::find(hardware, iface, instance)?
            .with_context(|| format!("{name} is not running; the phone's key service has to be started first"))?;
        parts.push((name.as_str(), handle));
    }
    let mut all = Vec::new();
    for (name, handle) in &parts {
        let reply = handle
            .transact(HARDWARE_GET_PARAMETERS, &Message::call(HARDWARE_INTERFACE))
            .with_context(|| format!("ask {name} what it brings to the agreement"))?;
        all.push(hardware_parameters(&mut Answer::new(reply.bytes()))?);
    }
    all.sort();
    let call = ask_hardware_compute(&all);
    let mut agreed = None;
    for (name, handle) in &parts {
        let reply = handle
            .transact(HARDWARE_COMPUTE, &call)
            .with_context(|| format!("have {name} work out the key it signs its notes with"))?;
        let said = hardware_agreed(&mut Answer::new(reply.bytes()))?;
        anyhow::ensure!(
            agreed.get_or_insert_with(|| said.clone()) == &said,
            "{name} worked out another key than the rest of the secure world"
        );
    }
    Ok(parts.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answered(fields: impl FnOnce(&mut Parcel)) -> Vec<u8> {
        let mut p = Parcel::default();
        p.i32(0);
        p.i32(1);
        let at = p.here();
        p.i32(0);
        fields(&mut p);
        let size = (p.here() - at) as i32;
        p.fill(at, size);
        p.bytes().to_vec()
    }

    #[test]
    fn a_part_brings_a_seed_it_keeps_and_a_number_it_drew() {
        let reply = answered(|p| {
            p.blob(&[1; 32]);
            p.blob(&[2; 32]);
        });
        let brought = parameters(&mut Reader::new(&reply)).unwrap();
        assert_eq!(brought, Parameters { seed: vec![1; 32], nonce: vec![2; 32] });
    }

    #[test]
    fn working_out_the_key_tells_the_secure_world_what_every_part_brought() {
        let all =
            [Parameters { seed: vec![], nonce: vec![9; 32] }, Parameters { seed: vec![4; 32], nonce: vec![5; 32] }];
        let call = ask_compute(&all);
        let mut r = Reader::new(call.bytes());
        for _ in 0..3 {
            r.i32().unwrap();
        }
        assert_eq!(r.string().unwrap(), INTERFACE);
        assert_eq!(r.i32().unwrap(), 2, "both parts are named");
        for p in &all {
            let end = r.structure().unwrap();
            assert_eq!(r.blob().unwrap(), p.seed);
            assert_eq!(r.blob().unwrap(), p.nonce);
            r.go(end).unwrap();
        }
        assert_eq!(r.here(), call.bytes().len());
    }

    #[test]
    fn the_values_are_given_in_the_order_they_sort_in() {
        let mut all = vec![
            Parameters { seed: vec![2], nonce: vec![1] },
            Parameters { seed: vec![1], nonce: vec![9] },
            Parameters { seed: vec![1], nonce: vec![3] },
        ];
        all.sort();
        assert_eq!(
            all,
            [
                Parameters { seed: vec![1], nonce: vec![3] },
                Parameters { seed: vec![1], nonce: vec![9] },
                Parameters { seed: vec![2], nonce: vec![1] },
            ]
        );
    }

    #[test]
    fn what_came_back_is_what_tells_the_parts_apart() {
        let mut p = Parcel::default();
        p.i32(0);
        p.blob(&[0xAB; 32]);
        assert_eq!(agreed(&mut Reader::new(p.bytes())).unwrap(), vec![0xAB; 32]);
    }
}
