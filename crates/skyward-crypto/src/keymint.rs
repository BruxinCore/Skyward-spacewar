use crate::token::Token;
use anyhow::{Context, Result};
use skyward_binder::driver::{Binder, Handle};
use skyward_binder::hidl::{self, Answer, Message};
use skyward_binder::parcel::Parcel;
use skyward_binder::{service, status};
use std::fmt;

pub const SERVICE: &str = "android.hardware.security.keymint.IKeyMintDevice/default";

const DEVICE: &str = "android.hardware.security.keymint.IKeyMintDevice";
const OPERATION: &str = "android.hardware.security.keymint.IKeyMintOperation";

const HARDWARE_SERVICES: [&str; 2] = [
    "android.hardware.keymaster@4.1::IKeymasterDevice",
    "android.hardware.keymaster@4.0::IKeymasterDevice",
];
const HARDWARE_DEVICE: &str = "android.hardware.keymaster@4.0::IKeymasterDevice";
const INSTANCE: &str = "default";

const GET_HARDWARE_INFO: u32 = 1;
const UPGRADE_KEY: u32 = 6;
const CONVERT_STORAGE_KEY: u32 = 13;
const GET_KEY_CHARACTERISTICS: u32 = 14;
const BEGIN: u32 = 10;

const HARDWARE_GET_INFO: u32 = 1;
const HARDWARE_GET_KEY_CHARACTERISTICS: u32 = 9;
const HARDWARE_EXPORT_KEY: u32 = 10;
const HARDWARE_UPGRADE_KEY: u32 = 12;
const HARDWARE_BEGIN: u32 = 16;
const HARDWARE_UPDATE: u32 = 17;
const HARDWARE_FINISH: u32 = 18;
const HARDWARE_ABORT: u32 = 19;

const KEY_FORMAT_RAW: u32 = 3;

const TAG_KIND: i32 = -268_435_456;
const KIND_ULONG: i32 = 1_342_177_280;
const KIND_ULONG_REP: i32 = -1_610_612_736;
const KIND_DATE: i32 = 1_610_612_736;
const KIND_BYTES: i32 = -1_879_048_192;
const KIND_BIGNUM: i32 = -2_147_483_648;
const KIND_BOOL: i32 = 1_879_048_192;
const UPDATE: u32 = 2;
const FINISH: u32 = 3;
const ABORT: u32 = 4;

const NONCE_BYTES: usize = 12;
const KEY_REQUIRES_UPGRADE: i32 = -62;

const PURPOSE_DECRYPT: i32 = 1;
const BLOCK_MODE_GCM: i32 = 32;
const PADDING_NONE: i32 = 1;

const TAG_PURPOSE: i32 = 536_870_913;
const TAG_BLOCK_MODE: i32 = 536_870_916;
const TAG_PADDING: i32 = 536_870_918;
const TAG_MAC_LENGTH: i32 = 805_307_371;
const TAG_APPLICATION_ID: i32 = -1_879_047_591;
const TAG_NONCE: i32 = -1_879_047_191;

const VALUE_PADDING_MODE: i32 = 3;
const VALUE_KEY_PURPOSE: i32 = 7;
const VALUE_INTEGER: i32 = 11;
const VALUE_BLOB: i32 = 14;
const VALUE_BLOCK_MODE: i32 = 2;

#[derive(Clone, Copy)]
enum Value<'a> {
    Enum(i32, i32),
    Integer(i32),
    Blob(&'a [u8]),
}

#[derive(Debug)]
struct HardwareRefused(i32);

impl fmt::Display for HardwareRefused {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "the key service refused the call with error {}", self.0)
    }
}

impl std::error::Error for HardwareRefused {}

fn hardware_ok(code: i32) -> Result<()> {
    if code == 0 {
        return Ok(());
    }
    Err(HardwareRefused(code).into())
}

fn refused_code(err: &anyhow::Error) -> Option<i32> {
    status::refused_with(err).or_else(|| err.downcast_ref::<HardwareRefused>().map(|r| r.0))
}

fn parameter(p: &mut Parcel, tag: i32, value: Value) {
    p.i32(1);
    let start = p.here();
    p.i32(0);
    p.i32(tag);
    p.i32(1);
    match value {
        Value::Enum(arm, v) => {
            p.i32(arm);
            p.i32(v);
        }
        Value::Integer(v) => {
            p.i32(VALUE_INTEGER);
            p.i32(v);
        }
        Value::Blob(v) => {
            p.i32(VALUE_BLOB);
            p.blob(v);
        }
    }
    let size = (p.here() - start) as i32;
    p.fill(start, size);
}

fn vouched(p: &mut Parcel, note: &Token) {
    p.i32(1);
    let start = p.here();
    p.i32(0);
    p.i64(note.challenge);
    p.i64(note.user);
    p.i64(note.authenticator);
    p.i32(note.kind);
    p.i32(1);
    let when = p.here();
    p.i32(0);
    p.i64(note.when);
    let size = (p.here() - when) as i32;
    p.fill(when, size);
    p.blob(&note.mac);
    let size = (p.here() - start) as i32;
    p.fill(start, size);
}

fn older_key_blob(key: &[u8]) -> &[u8] {
    key.strip_prefix(b"pKMblob\0").unwrap_or(key)
}

fn write_older_params(call: &mut Message, params: &[(i32, Value)]) {
    let mut raw = vec![0u8; params.len() * 32];
    let mut blobs = Vec::with_capacity(params.len());
    for (i, (tag, value)) in params.iter().enumerate() {
        let entry = &mut raw[i * 32..(i + 1) * 32];
        entry[0..4].copy_from_slice(&tag.to_le_bytes());
        match value {
            Value::Enum(_, v) | Value::Integer(v) => {
                entry[8..12].copy_from_slice(&v.to_le_bytes());
                blobs.push((i * 32 + 16, 0u64, 0usize));
            }
            Value::Blob(b) => {
                let at = call.keep(b.to_vec());
                entry[16..24].copy_from_slice(&at.to_le_bytes());
                entry[24..28].copy_from_slice(&(b.len() as u32).to_le_bytes());
                blobs.push((i * 32 + 16, at, b.len()));
            }
        }
    }
    let parent = call.framed_vec(raw, params.len());
    for (offset, at, len) in blobs {
        call.address_at(at, len, Some((parent, offset)));
    }
}

fn write_older_auth_token(call: &mut Message, note: Option<&Token>) {
    let mut raw = vec![0u8; 56];
    let (mac_at, mac_len) = match note {
        Some(n) => {
            raw[0..8].copy_from_slice(&n.challenge.to_le_bytes());
            raw[8..16].copy_from_slice(&n.user.to_le_bytes());
            raw[16..24].copy_from_slice(&n.authenticator.to_le_bytes());
            raw[24..28].copy_from_slice(&n.kind.to_le_bytes());
            raw[32..40].copy_from_slice(&n.when.to_le_bytes());
            let at = call.keep(n.mac.clone());
            raw[40..48].copy_from_slice(&at.to_le_bytes());
            raw[48..52].copy_from_slice(&(n.mac.len() as u32).to_le_bytes());
            (at, n.mac.len())
        }
        None => (0u64, 0usize),
    };
    let raw_at = call.keep(raw);
    let parent = call.address_at(raw_at, 56, None);
    call.address_at(mac_at, mac_len, Some((parent, 40)));
}

fn write_older_verification_token(call: &mut Message) {
    let raw_at = call.keep(vec![0u8; 56]);
    let parent = call.address_at(raw_at, 56, None);
    call.address_at(0, 0, Some((parent, 16)));
    call.address_at(0, 0, Some((parent, 40)));
}

fn skip_older_params(read: &mut Answer) -> Result<()> {
    let frame = read.pointed().context("read the parameter frame")?;
    if frame.len() < 16 {
        anyhow::bail!("the key service answered a parameter frame of {} bytes", frame.len());
    }
    let count = u32::from_le_bytes(frame[8..12].try_into().expect("four bytes")) as usize;
    read.pointed_slice().context("read the parameters")?;
    for _ in 0..count {
        read.pointed_slice().context("read a parameter blob")?;
    }
    Ok(())
}

pub struct Hardware {
    pub version: i32,
    pub security_level: i32,
    pub name: String,
    pub author: String,
}

pub enum KeyMint<'a> {
    Newer(Handle<'a>),
    Older { device: Handle<'a>, version: i32 },
}

impl<'a> KeyMint<'a> {
    pub fn open(binder: &'a Binder, hardware: &'a Binder) -> Result<KeyMint<'a>> {
        let not_running = |name: &str| format!("{name} is not running; the phone's key service has to be started first");
        if service::declared(binder, SERVICE).unwrap_or(false) {
            let device = service::find(binder, SERVICE)?.with_context(|| not_running(SERVICE))?;
            return Ok(KeyMint::Newer(device));
        }
        for name in HARDWARE_SERVICES {
            if let Some(device) = hidl::find(hardware, name, INSTANCE)? {
                let version = if name.contains("@4.1") { 41 } else { 40 };
                return Ok(KeyMint::Older { device, version });
            }
        }
        anyhow::bail!("{}", not_running(SERVICE))
    }

    pub fn hardware(&self) -> Result<Hardware> {
        match self {
            KeyMint::Newer(device) => {
                let reply = device.transact(GET_HARDWARE_INFO, &Parcel::call(DEVICE)).context("ask the key service about itself")?;
                let mut read = reply.read();
                status::ok(&mut read).context("ask the key service about itself")?;
                let end = read.structure()?;
                let info = Hardware {
                    version: read.i32()?,
                    security_level: read.i32()?,
                    name: read.string()?,
                    author: read.string()?,
                };
                read.go(end)?;
                Ok(info)
            }
            KeyMint::Older { device, version } => {
                let reply = device.transact(HARDWARE_GET_INFO, &Message::call(HARDWARE_DEVICE)).context("ask the key service about itself")?;
                let mut read = Answer::new(reply.bytes());
                read.status().context("ask the key service about itself")?;
                let security_level = read.i32()?;
                let name = read.vec_string()?;
                let author = read.vec_string()?;
                Ok(Hardware { version: *version, security_level, name, author })
            }
        }
    }

    pub fn ephemeral_storage_key(&self, key: &[u8]) -> Result<Vec<u8>> {
        match self.convert(key) {
            Err(e) if refused_code(&e) == Some(KEY_REQUIRES_UPGRADE) => {
                let renewed = match self {
                    KeyMint::Newer(_) => self.upgrade(key, |call| call.i32(0)),
                    KeyMint::Older { .. } => self.upgrade_older(key, &[]),
                }
                .context("renew the storage key for the firmware running now")?;
                self.convert(&renewed)
            }
            converted => converted,
        }
    }

    fn convert(&self, key: &[u8]) -> Result<Vec<u8>> {
        match self {
            KeyMint::Newer(device) => {
                let mut call = Parcel::call(DEVICE);
                call.blob(key);
                let reply = device.transact(CONVERT_STORAGE_KEY, &call).context("make a storage key for this start")?;
                let mut read = reply.read();
                status::ok(&mut read).context("make a storage key for this start")?;
                read.blob().context("read the storage key")
            }
            KeyMint::Older { device, .. } => {
                let mut call = Message::call(HARDWARE_DEVICE);
                call.u32(KEY_FORMAT_RAW);
                call.bytes(older_key_blob(key));
                call.bytes(&[]);
                call.bytes(&[]);
                let reply = device.transact(HARDWARE_EXPORT_KEY, &call).context("make a storage key for this start")?;
                let mut read = Answer::new(reply.bytes());
                read.status().context("make a storage key for this start")?;
                let err = read.i32().context("make a storage key for this start")?;
                let material = read.vec_bytes().context("read the storage key")?.to_vec();
                hardware_ok(err).context("make a storage key for this start")?;
                Ok(material)
            }
        }
    }

    pub fn authorizations(&self, key_blob: &[u8], app_id: &[u8]) -> Result<Vec<(i32, i64)>> {
        match self {
            KeyMint::Newer(device) => {
                let mut call = Parcel::call(DEVICE);
                call.blob(key_blob);
                call.blob(app_id);
                call.blob(&[]);
                let reply = device.transact(GET_KEY_CHARACTERISTICS, &call).context("ask what the key may be used for")?;
                let mut read = reply.read();
                status::ok(&mut read).context("ask what the key may be used for")?;
                let mut out = Vec::new();
                for _ in 0..read.i32()?.max(0) {
                    let end = read.structure()?;
                    read.i32().context("read which part of the phone holds the key")?;
                    for _ in 0..read.i32()?.max(0) {
                        let next = read.structure()?;
                        let tag = read.i32()?;
                        read.i32().context("read whether the tag has a value")?;
                        read.i32().context("read which arm of the value is set")?;
                        let value = match tag & TAG_KIND {
                            KIND_ULONG | KIND_ULONG_REP | KIND_DATE => read.i64()?,
                            KIND_BYTES | KIND_BIGNUM => read.blob()?.len() as i64,
                            _ => read.i32()? as i64,
                        };
                        out.push((tag, value));
                        read.go(next)?;
                    }
                    read.go(end)?;
                }
                Ok(out)
            }
            KeyMint::Older { device, .. } => {
                let mut call = Message::call(HARDWARE_DEVICE);
                call.bytes(older_key_blob(key_blob));
                call.bytes(app_id);
                call.bytes(&[]);
                let reply = device.transact(HARDWARE_GET_KEY_CHARACTERISTICS, &call).context("ask what the key may be used for")?;
                let mut read = Answer::new(reply.bytes());
                read.status().context("ask what the key may be used for")?;
                let err = read.i32().context("ask what the key may be used for")?;
                hardware_ok(err).context("ask what the key may be used for")?;
                let kc = read.pointed().context("read what the key may be used for")?;
                if kc.len() < 32 {
                    anyhow::bail!("the key service answered {} bytes of key characteristics", kc.len());
                }
                let sw_count = u32::from_le_bytes(kc[8..12].try_into().expect("four bytes")) as usize;
                let hw_count = u32::from_le_bytes(kc[24..28].try_into().expect("four bytes")) as usize;
                let mut out = Vec::new();
                for count in [sw_count, hw_count] {
                    let arr = read.pointed_slice().context("read key parameters")?;
                    if arr.len() < count * 32 {
                        anyhow::bail!("the key service answered {} bytes for {count} parameters", arr.len());
                    }
                    for i in 0..count {
                        let entry = &arr[i * 32..(i + 1) * 32];
                        let tag = i32::from_le_bytes(entry[0..4].try_into().expect("four bytes"));
                        let blob = read.pointed_slice().context("read a key parameter blob")?;
                        let value = match tag & TAG_KIND {
                            KIND_ULONG | KIND_ULONG_REP | KIND_DATE => i64::from_le_bytes(entry[8..16].try_into().expect("eight bytes")),
                            KIND_BYTES | KIND_BIGNUM => blob.len() as i64,
                            KIND_BOOL => i64::from(entry[8] != 0),
                            _ => i32::from_le_bytes(entry[8..12].try_into().expect("four bytes")) as i64,
                        };
                        out.push((tag, value));
                    }
                }
                Ok(out)
            }
        }
    }

    pub fn gcm_decrypt(&self, key_blob: &[u8], app_id: &[u8], nonce: &[u8], body: &[u8], note: Option<&Token>) -> Result<Vec<u8>> {
        match self {
            KeyMint::Newer(_) => {
                let operation = match self.begin(key_blob, app_id, nonce, note) {
                    Ok(operation) => operation,
                    Err(e) if refused_code(&e) == Some(KEY_REQUIRES_UPGRADE) => {
                        let renewed = self
                            .upgrade(key_blob, |call| Self::gcm_params(call, app_id, nonce))
                            .context("renew the key for the firmware running now")?;
                        self.begin(&renewed, app_id, nonce, note)?
                    }
                    Err(e) => return Err(e),
                };
                let mut out = self.update(&operation, body)?;
                out.extend_from_slice(&self.finish(&operation)?);
                Ok(out)
            }
            KeyMint::Older { .. } => {
                let params = Self::older_gcm_params(app_id, nonce);
                let op = match self.begin_older(key_blob, &params, note) {
                    Ok(op) => op,
                    Err(e) if refused_code(&e) == Some(KEY_REQUIRES_UPGRADE) => {
                        let renewed = self
                            .upgrade_older(key_blob, &params)
                            .context("renew the key for the firmware running now")?;
                        self.begin_older(&renewed, &params, note)?
                    }
                    Err(e) => return Err(e),
                };
                let run = (|| -> Result<Vec<u8>> {
                    let mut out = self.update_older(op, body, note)?;
                    out.extend_from_slice(&self.finish_older(op, note)?);
                    Ok(out)
                })();
                if run.is_err() {
                    let _ = self.abort_older(op);
                }
                run
            }
        }
    }

    fn older_gcm_params<'b>(app_id: &'b [u8], nonce: &'b [u8]) -> Vec<(i32, Value<'b>)> {
        let mut out = vec![
            (TAG_BLOCK_MODE, Value::Enum(VALUE_BLOCK_MODE, BLOCK_MODE_GCM)),
            (TAG_PADDING, Value::Enum(VALUE_PADDING_MODE, PADDING_NONE)),
            (TAG_MAC_LENGTH, Value::Integer(128)),
        ];
        if !app_id.is_empty() {
            out.push((TAG_APPLICATION_ID, Value::Blob(app_id)));
        }
        out.push((TAG_NONCE, Value::Blob(nonce)));
        out
    }

    fn gcm_params(call: &mut Parcel, app_id: &[u8], nonce: &[u8]) {
        call.i32(if app_id.is_empty() { 5 } else { 6 });
        parameter(call, TAG_BLOCK_MODE, Value::Enum(VALUE_BLOCK_MODE, BLOCK_MODE_GCM));
        parameter(call, TAG_PADDING, Value::Enum(VALUE_PADDING_MODE, PADDING_NONE));
        parameter(call, TAG_MAC_LENGTH, Value::Integer(128));
        if !app_id.is_empty() {
            parameter(call, TAG_APPLICATION_ID, Value::Blob(app_id));
        }
        parameter(call, TAG_NONCE, Value::Blob(nonce));
        parameter(call, TAG_PURPOSE, Value::Enum(VALUE_KEY_PURPOSE, PURPOSE_DECRYPT));
    }

    fn begin(&self, key_blob: &[u8], app_id: &[u8], nonce: &[u8], note: Option<&Token>) -> Result<Handle<'a>> {
        let KeyMint::Newer(device) = self else { unreachable!() };
        let mut call = Parcel::call(DEVICE);
        call.i32(PURPOSE_DECRYPT);
        call.blob(key_blob);
        Self::gcm_params(&mut call, app_id, nonce);
        match note {
            Some(note) => vouched(&mut call, note),
            None => call.none(),
        }
        let reply = device.transact(BEGIN, &call).context("start a decryption")?;
        let mut read = reply.read();
        status::ok(&mut read).context("start a decryption")?;
        read.structure().context("read what the key service answered")?;
        read.i64().context("read the decryption's challenge")?;
        let params = read.i32().context("read how many parameters the decryption came back with")?;
        for _ in 0..params.max(0) {
            let end = read.structure().context("read a parameter the decryption came back with")?;
            read.go(end)?;
        }
        match read.object().context("read the decryption the key service opened")? {
            Some(h) => Handle::hold(reply.binder(), h),
            None => anyhow::bail!("the key service opened no decryption"),
        }
    }

    fn begin_older(&self, key_blob: &[u8], params: &[(i32, Value)], note: Option<&Token>) -> Result<u64> {
        let KeyMint::Older { device, .. } = self else { unreachable!() };
        let mut call = Message::call(HARDWARE_DEVICE);
        call.i32(PURPOSE_DECRYPT);
        call.bytes(older_key_blob(key_blob));
        write_older_params(&mut call, params);
        write_older_auth_token(&mut call, note);
        let reply = device.transact(HARDWARE_BEGIN, &call).context("start a decryption")?;
        let mut read = Answer::new(reply.bytes());
        read.status().context("start a decryption")?;
        let err = read.i32().context("start a decryption")?;
        skip_older_params(&mut read)?;
        let handle = read.u64().context("read the decryption the key service opened")?;
        hardware_ok(err).context("start a decryption")?;
        Ok(handle)
    }

    fn upgrade(&self, key_blob: &[u8], params: impl FnOnce(&mut Parcel)) -> Result<Vec<u8>> {
        let KeyMint::Newer(device) = self else { unreachable!() };
        let mut call = Parcel::call(DEVICE);
        call.blob(key_blob);
        params(&mut call);
        let reply = device.transact(UPGRADE_KEY, &call).context("renew the key")?;
        let mut read = reply.read();
        status::ok(&mut read).context("renew the key")?;
        read.blob().context("read the renewed key")
    }

    fn upgrade_older(&self, key_blob: &[u8], params: &[(i32, Value)]) -> Result<Vec<u8>> {
        let KeyMint::Older { device, .. } = self else { unreachable!() };
        let mut call = Message::call(HARDWARE_DEVICE);
        call.bytes(older_key_blob(key_blob));
        write_older_params(&mut call, params);
        let reply = device.transact(HARDWARE_UPGRADE_KEY, &call).context("renew the key")?;
        let mut read = Answer::new(reply.bytes());
        read.status().context("renew the key")?;
        let err = read.i32().context("renew the key")?;
        let renewed = read.vec_bytes().context("read the renewed key")?.to_vec();
        hardware_ok(err).context("renew the key")?;
        Ok(renewed)
    }

    fn update(&self, operation: &Handle, input: &[u8]) -> Result<Vec<u8>> {
        let mut call = Parcel::call(OPERATION);
        call.blob(input);
        call.none();
        call.none();
        let reply = operation.transact(UPDATE, &call).context("feed the decryption")?;
        let mut read = reply.read();
        status::ok(&mut read).context("feed the decryption")?;
        read.blob().context("read what the decryption gave back")
    }

    fn update_older(&self, operation: u64, input: &[u8], note: Option<&Token>) -> Result<Vec<u8>> {
        let KeyMint::Older { device, .. } = self else { unreachable!() };
        let mut out = Vec::new();
        let mut at = 0;
        while at < input.len() {
            let mut call = Message::call(HARDWARE_DEVICE);
            call.u64(operation);
            write_older_params(&mut call, &[]);
            call.bytes(&input[at..]);
            write_older_auth_token(&mut call, note);
            write_older_verification_token(&mut call);
            let reply = device.transact(HARDWARE_UPDATE, &call).context("feed the decryption")?;
            let mut read = Answer::new(reply.bytes());
            read.status().context("feed the decryption")?;
            let err = read.i32().context("feed the decryption")?;
            let consumed = read.u32().context("read how much of the input the decryption took")? as usize;
            skip_older_params(&mut read)?;
            let chunk = read.vec_bytes().context("read what the decryption gave back")?;
            hardware_ok(err).context("feed the decryption")?;
            out.extend_from_slice(chunk);
            if consumed == 0 {
                anyhow::bail!("the key service took none of the input it was given");
            }
            at += consumed;
        }
        Ok(out)
    }

    fn finish(&self, operation: &Handle) -> Result<Vec<u8>> {
        let mut call = Parcel::call(OPERATION);
        call.opt_blob(None);
        call.opt_blob(None);
        call.none();
        call.none();
        call.opt_blob(None);
        let reply = operation.transact(FINISH, &call).context("close the decryption")?;
        let mut read = reply.read();
        status::ok(&mut read).context("close the decryption, which is where a wrong key is refused")?;
        read.blob().context("read what the decryption ended with")
    }

    fn finish_older(&self, operation: u64, note: Option<&Token>) -> Result<Vec<u8>> {
        let KeyMint::Older { device, .. } = self else { unreachable!() };
        let mut call = Message::call(HARDWARE_DEVICE);
        call.u64(operation);
        write_older_params(&mut call, &[]);
        call.bytes(&[]);
        call.bytes(&[]);
        write_older_auth_token(&mut call, note);
        write_older_verification_token(&mut call);
        let reply = device.transact(HARDWARE_FINISH, &call).context("close the decryption")?;
        let mut read = Answer::new(reply.bytes());
        read.status().context("close the decryption, which is where a wrong key is refused")?;
        let err = read.i32().context("close the decryption, which is where a wrong key is refused")?;
        skip_older_params(&mut read)?;
        let out = read.vec_bytes().context("read what the decryption ended with")?.to_vec();
        hardware_ok(err).context("close the decryption, which is where a wrong key is refused")?;
        Ok(out)
    }

    fn abort(&self, operation: &Handle) -> Result<()> {
        let reply = operation.transact(ABORT, &Parcel::call(OPERATION)).context("drop the decryption")?;
        status::ok(&mut reply.read()).context("drop the decryption")
    }

    fn abort_older(&self, operation: u64) -> Result<()> {
        let KeyMint::Older { device, .. } = self else { unreachable!() };
        let mut call = Message::call(HARDWARE_DEVICE);
        call.u64(operation);
        let reply = device.transact(HARDWARE_ABORT, &call).context("drop the decryption")?;
        let mut read = Answer::new(reply.bytes());
        read.status().context("drop the decryption")?;
        let err = read.i32().context("drop the decryption")?;
        hardware_ok(err).context("drop the decryption")
    }

    pub fn usable(&self, key_blob: &[u8], app_id: &[u8]) -> Result<()> {
        match self {
            KeyMint::Newer(_) => {
                let operation = self.begin(key_blob, app_id, &[0; NONCE_BYTES], None)?;
                self.abort(&operation)
            }
            KeyMint::Older { .. } => {
                let params = Self::older_gcm_params(app_id, &[0; NONCE_BYTES]);
                let operation = self.begin_older(key_blob, &params, None)?;
                self.abort_older(operation)
            }
        }
    }
}
