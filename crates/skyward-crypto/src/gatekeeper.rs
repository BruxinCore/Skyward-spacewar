#![cfg_attr(not(target_os = "android"), allow(dead_code))]

use crate::token::Token;
use anyhow::{bail, Context, Result};
use skyward_binder::hidl::{Answer, Message};
use skyward_binder::parcel::{Parcel, Reader};
use skyward_binder::status;

pub const SERVICE: &str = "android.hardware.gatekeeper.IGatekeeper/default";
const INTERFACE: &str = "android.hardware.gatekeeper.IGatekeeper";

pub const HARDWARE_SERVICE: &str = "android.hardware.gatekeeper@1.0::IGatekeeper";
const INSTANCE: &str = "default";

const VERIFY: u32 = 4;
const HARDWARE_VERIFY: u32 = 2;

const RIGHT: i32 = 0;
const RIGHT_BUT_RENEW: i32 = 1;
const HOLDING_OFF: i32 = -2;
const RETRY_TIMEOUT: i32 = -2;
const ANSWER_BYTES: usize = 24;
const NOTE_LENGTH: std::ops::Range<usize> = 16..20;

fn ask(slot: u32, enrolled: &[u8], password: &[u8]) -> Parcel {
    let mut call = Parcel::call(INTERFACE);
    call.i32(slot as i32);
    call.i64(0);
    call.blob(enrolled);
    call.blob(password);
    call
}

fn note(read: &mut Reader) -> Result<Token> {
    if let Err(e) = status::ok(read) {
        return Err(match status::refused_with(&e) {
            Some(RETRY_TIMEOUT) => e.context("the phone is holding off further tries"),
            Some(_) => e.context("the secure world does not recognise that credential"),
            None => e.context("have the secure world check the credential"),
        });
    }
    let end = read.structure().context("read what the secure world said about the credential")?;
    read.i32().context("read how the secure world took the credential")?;
    read.i32().context("read how long the secure world asks to wait")?;
    let note = written(read).context("read the note the secure world wrote")?;
    read.go(end)?;
    Ok(note)
}

fn written(read: &mut Reader) -> Result<Token> {
    let end = read.structure()?;
    let challenge = read.i64()?;
    let user = read.i64()?;
    let authenticator = read.i64()?;
    let kind = read.i32()?;
    let at = read.structure()?;
    let when = read.i64()?;
    read.go(at)?;
    let mac = read.blob()?;
    read.go(end)?;
    if mac.is_empty() {
        bail!("the secure world took the credential but wrote no note for the key service");
    }
    Ok(Token { challenge, user, authenticator, kind, when, mac })
}

fn ask_hardware(slot: u32, enrolled: &[u8], password: &[u8]) -> Message {
    let mut call = Message::call(HARDWARE_SERVICE);
    call.u32(slot);
    call.u64(0);
    call.bytes(enrolled);
    call.bytes(password);
    call
}

fn hardware_note(read: &mut Answer) -> Result<Token> {
    read.status().context("have the secure world check the credential")?;
    let answer = read.pointed().context("read what the secure world said about the credential")?;
    if answer.len() < ANSWER_BYTES {
        bail!("the secure world answered {} bytes about the credential", answer.len());
    }
    let said = i32::from_le_bytes(answer[0..4].try_into().expect("four bytes"));
    let wait = i32::from_le_bytes(answer[4..8].try_into().expect("four bytes"));
    let length = u32::from_le_bytes(answer[NOTE_LENGTH].try_into().expect("four bytes"));
    match said {
        RIGHT | RIGHT_BUT_RENEW => {}
        HOLDING_OFF => bail!("the phone is holding off further tries for another {} seconds", (wait + 999) / 1000),
        _ => bail!("the secure world does not recognise that credential"),
    }
    if length == 0 {
        bail!("the secure world took the credential but wrote no note for the key service");
    }
    let note = read.pointed().context("read the note the secure world wrote")?;
    Token::of(note)
}

#[cfg(target_os = "android")]
pub enum Gatekeeper<'a> {
    Newer(skyward_binder::driver::Handle<'a>),
    Older(skyward_binder::driver::Handle<'a>),
}

#[cfg(target_os = "android")]
impl<'a> Gatekeeper<'a> {
    pub fn open(binder: &'a skyward_binder::driver::Binder, hardware: &'a skyward_binder::driver::Binder) -> Result<Gatekeeper<'a>> {
        use skyward_binder::{hidl, service};
        let not_running = |name: &str| format!("{name} is not running; the phone's credential service has to be started first");
        if service::declared(binder, SERVICE).unwrap_or(false) {
            let device = service::find(binder, SERVICE)?.with_context(|| not_running(SERVICE))?;
            return Ok(Gatekeeper::Newer(device));
        }
        let device = hidl::find(hardware, HARDWARE_SERVICE, INSTANCE)?.with_context(|| not_running(HARDWARE_SERVICE))?;
        Ok(Gatekeeper::Older(device))
    }

    pub fn serves(&self) -> Result<String> {
        match self {
            Gatekeeper::Newer(device) => skyward_binder::service::interface(device),
            Gatekeeper::Older(device) => skyward_binder::hidl::describes(device),
        }
    }

    pub fn version(&self) -> Result<Option<i32>> {
        match self {
            Gatekeeper::Newer(device) => skyward_binder::service::version(device, INTERFACE).map(Some),
            Gatekeeper::Older(_) => Ok(None),
        }
    }

    pub fn verify(&self, slot: u32, enrolled: &[u8], password: &[u8]) -> Result<Token> {
        let what = "have the secure world check the credential";
        match self {
            Gatekeeper::Newer(device) => {
                let reply = device.transact(VERIFY, &ask(slot, enrolled, password)).context(what)?;
                note(&mut reply.read())
            }
            Gatekeeper::Older(device) => {
                let reply = device.transact(HARDWARE_VERIFY, &ask_hardware(slot, enrolled, password)).context(what)?;
                hardware_note(&mut Answer::new(reply.bytes()))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn structure(p: &mut Parcel, fields: impl FnOnce(&mut Parcel)) {
        p.i32(1);
        let at = p.here();
        p.i32(0);
        fields(p);
        let size = (p.here() - at) as i32;
        p.fill(at, size);
    }

    fn answered(mac: &[u8]) -> Vec<u8> {
        let mut p = Parcel::default();
        p.i32(0);
        structure(&mut p, |p| {
            p.i32(0);
            p.i32(0);
            structure(p, |p| {
                p.i64(0);
                p.i64(0x253c_22c3_acbb_a741);
                p.i64(9);
                p.i32(1);
                structure(p, |p| p.i64(1_700_000_000));
                p.blob(mac);
            });
        });
        p.bytes().to_vec()
    }

    fn refused(code: i32) -> Vec<u8> {
        let mut p = Parcel::default();
        p.i32(-8);
        p.string("");
        p.i32(0);
        p.i32(code);
        p.bytes().to_vec()
    }

    #[test]
    fn the_newer_kind_is_asked_with_the_slot_the_enrolment_and_the_password() {
        let call = ask(100_000, &[1; 58], &[2; 64]);
        let mut r = Reader::new(call.bytes());
        for _ in 0..3 {
            r.i32().unwrap();
        }
        assert_eq!(r.string().unwrap(), INTERFACE);
        assert_eq!(r.i32().unwrap(), 100_000);
        assert_eq!(r.i64().unwrap(), 0, "no challenge");
        assert_eq!(r.blob().unwrap(), vec![1; 58]);
        assert_eq!(r.blob().unwrap(), vec![2; 64]);
        assert_eq!(r.here(), call.bytes().len());
    }

    #[test]
    fn the_newer_kind_hands_over_the_note_field_by_field() {
        let reply = answered(&[0xAB; 32]);
        let token = note(&mut Reader::new(&reply)).unwrap();
        assert_eq!(
            token,
            Token { challenge: 0, user: 0x253c_22c3_acbb_a741, authenticator: 9, kind: 1, when: 1_700_000_000, mac: vec![0xAB; 32] }
        );
    }

    #[test]
    fn the_newer_kinds_refusals_say_what_they_mean() {
        let wrong = note(&mut Reader::new(&refused(-1))).unwrap_err();
        assert!(format!("{wrong:#}").starts_with("the secure world does not recognise that credential"));
        let waiting = note(&mut Reader::new(&refused(RETRY_TIMEOUT))).unwrap_err();
        assert!(format!("{waiting:#}").starts_with("the phone is holding off further tries"));
        assert!(note(&mut Reader::new(&answered(&[]))).is_err(), "a right answer with no note");
    }
}
