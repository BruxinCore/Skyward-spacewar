use crate::parcel::Payload;
use anyhow::{bail, Context, Result};

const ADDRESS: u32 = u32::from_be_bytes([b'p', b't', b'*', 0x85]);
const OBJECT: u32 = u32::from_be_bytes([b's', b'h', b'*', 0x85]);
const INSIDE_ANOTHER: u32 = 1;
const ADDRESS_BYTES: usize = 40;
const OBJECT_BYTES: usize = 24;
const FRAME_BYTES: usize = 16;
const WORD: u64 = 8;

#[derive(Default)]
pub struct Message {
    data: Vec<u8>,
    addresses: Vec<u64>,
    held: Vec<Box<[u8]>>,
    pointed: u64,
}

impl Message {
    pub fn call(interface: &str) -> Message {
        let mut m = Message::default();
        m.data.extend_from_slice(interface.as_bytes());
        m.data.push(0);
        while !m.data.len().is_multiple_of(4) {
            m.data.push(0);
        }
        m
    }

    pub fn i32(&mut self, v: i32) {
        self.u32(v as u32);
    }

    pub fn u32(&mut self, v: u32) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    pub fn u64(&mut self, v: u64) {
        self.data.extend_from_slice(&v.to_le_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        let at = self.keep(value.to_vec());
        self.framed(at, value.len(), value.len());
    }

    pub fn text(&mut self, value: &str) {
        let mut raw = value.as_bytes().to_vec();
        raw.push(0);
        let at = self.keep(raw);
        self.framed(at, value.len(), value.len() + 1);
    }

    pub fn keep(&mut self, bytes: Vec<u8>) -> u64 {
        let bytes = bytes.into_boxed_slice();
        let at = bytes.as_ptr() as u64;
        self.held.push(bytes);
        at
    }

    pub fn framed_vec(&mut self, elements: Vec<u8>, count: usize) -> usize {
        let len = elements.len();
        let at = self.keep(elements);
        self.framed(at, count, len)
    }

    fn framed(&mut self, at: u64, count: usize, length: usize) -> usize {
        let mut frame = Vec::with_capacity(FRAME_BYTES);
        frame.extend_from_slice(&at.to_le_bytes());
        frame.extend_from_slice(&(count as u32).to_le_bytes());
        frame.extend_from_slice(&[0; 4]);
        let frame_at = self.keep(frame);
        let parent = self.address_at(frame_at, FRAME_BYTES, None);
        self.address_at(at, length, Some((parent, 0)))
    }

    pub fn address_at(&mut self, at: u64, length: usize, inside: Option<(usize, usize)>) -> usize {
        let which = self.addresses.len();
        self.addresses.push(self.data.len() as u64);
        self.data.extend_from_slice(&ADDRESS.to_le_bytes());
        self.data.extend_from_slice(&inside.map_or(0, |_| INSIDE_ANOTHER).to_le_bytes());
        self.data.extend_from_slice(&at.to_le_bytes());
        self.data.extend_from_slice(&(length as u64).to_le_bytes());
        self.data.extend_from_slice(&(inside.map_or(0, |(p, _)| p) as u64).to_le_bytes());
        self.data.extend_from_slice(&(inside.map_or(0, |(_, o)| o) as u64).to_le_bytes());
        self.pointed += (length as u64).next_multiple_of(WORD);
        which
    }
}

impl Payload for Message {
    fn values(&self) -> &[u8] {
        &self.data
    }

    fn addresses(&self) -> &[u64] {
        &self.addresses
    }

    fn pointed(&self) -> u64 {
        self.pointed
    }
}

pub struct Answer<'a> {
    data: &'a [u8],
    at: usize,
}

impl<'a> Answer<'a> {
    pub fn new(data: &'a [u8]) -> Answer<'a> {
        Answer { data, at: 0 }
    }

    pub fn status(&mut self) -> Result<()> {
        let code = self.i32().context("read whether the service took the call")?;
        if code == 0 {
            return Ok(());
        }
        match self.text() {
            Ok(why) if !why.is_empty() => bail!("the service refused the call: {why}"),
            _ => bail!("the service refused the call with its code {code}"),
        }
    }

    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().expect("four bytes")))
    }

    pub fn u64(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().expect("eight bytes")))
    }

    pub fn bool(&mut self) -> Result<bool> {
        Ok(self.take(4)?[0] != 0)
    }

    pub fn pointed(&mut self) -> Result<&'a [u8]> {
        let raw = self.take(ADDRESS_BYTES)?;
        let kind = u32::from_le_bytes(raw[0..4].try_into().expect("four bytes"));
        if kind != ADDRESS {
            bail!("the reply holds {kind:#x} where an address was expected");
        }
        let at = u64::from_le_bytes(raw[8..16].try_into().expect("eight bytes"));
        let length = u64::from_le_bytes(raw[16..24].try_into().expect("eight bytes"));
        if at == 0 {
            bail!("the service left out something the call asked for");
        }
        Ok(unsafe { std::slice::from_raw_parts(at as *const u8, length as usize) })
    }

    pub fn pointed_slice(&mut self) -> Result<&'a [u8]> {
        let raw = self.take(ADDRESS_BYTES)?;
        let kind = u32::from_le_bytes(raw[0..4].try_into().expect("four bytes"));
        if kind != ADDRESS {
            bail!("the reply holds {kind:#x} where an address was expected");
        }
        let at = u64::from_le_bytes(raw[8..16].try_into().expect("eight bytes"));
        let length = u64::from_le_bytes(raw[16..24].try_into().expect("eight bytes"));
        if length == 0 {
            return Ok(&[]);
        }
        if at == 0 {
            bail!("the service left out something the call asked for");
        }
        Ok(unsafe { std::slice::from_raw_parts(at as *const u8, length as usize) })
    }

    pub fn vec_bytes(&mut self) -> Result<&'a [u8]> {
        let frame = self.pointed()?;
        if frame.len() < FRAME_BYTES {
            bail!("the service answered a frame of {} bytes", frame.len());
        }
        let count = u32::from_le_bytes(frame[8..12].try_into().expect("four bytes")) as usize;
        let bytes = self.pointed_slice()?;
        Ok(&bytes[..count.min(bytes.len())])
    }

    pub fn vec_string(&mut self) -> Result<String> {
        let bytes = self.vec_bytes()?;
        Ok(String::from_utf8_lossy(bytes.strip_suffix(&[0]).unwrap_or(bytes)).into_owned())
    }

    fn text(&mut self) -> Result<String> {
        let units = self.i32()?;
        if units < 0 {
            bail!("text of length {units} was read where some was expected");
        }
        let raw = self.take((units as usize + 1) * 2)?;
        while !self.at.is_multiple_of(4) && self.at < self.data.len() {
            self.at += 1;
        }
        let utf16: Vec<u16> = raw[..units as usize * 2].as_chunks::<2>().0.iter().map(|c| u16::from_le_bytes(*c)).collect();
        Ok(String::from_utf16_lossy(&utf16))
    }

    pub fn object(&mut self) -> Result<Option<u32>> {
        let raw = self.take(OBJECT_BYTES)?;
        let kind = u32::from_le_bytes(raw[0..4].try_into().expect("four bytes"));
        let handle = u32::from_le_bytes(raw[8..12].try_into().expect("four bytes"));
        Ok((kind == OBJECT).then_some(handle))
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.at + n;
        if end > self.data.len() {
            bail!("the reply holds {} bytes, {n} more were read at {}", self.data.len(), self.at);
        }
        let out = &self.data[self.at..end];
        self.at = end;
        Ok(out)
    }
}

#[cfg(target_os = "android")]
pub fn find<'a>(binder: &'a crate::driver::Binder, interface: &str, instance: &str) -> Result<Option<crate::driver::Handle<'a>>> {
    use crate::driver::Handle;
    const DIRECTORY: &str = "android.hidl.manager@1.0::IServiceManager";
    const GET: u32 = 1;
    let directory = binder.directory()?;
    let mut call = Message::call(DIRECTORY);
    call.text(interface);
    call.text(instance);
    let reply = directory.transact(GET, &call).with_context(|| format!("look up {interface}/{instance}"))?;
    let mut read = Answer::new(reply.bytes());
    read.status().with_context(|| format!("look up {interface}/{instance}"))?;
    match read.object().with_context(|| format!("read what the directory answered for {interface}"))? {
        Some(handle) => Ok(Some(Handle::hold(reply.binder(), handle)?)),
        None => Ok(None),
    }
}

#[cfg(target_os = "android")]
pub fn describes(service: &crate::driver::Handle) -> Result<String> {
    const BASE: &str = "android.hidl.base@1.0::IBase";
    const DESCRIBE: u32 = 256_136_003;
    let reply = service.transact(DESCRIBE, &Message::call(BASE)).context("ask the service what it serves")?;
    let mut read = Answer::new(reply.bytes());
    read.status().context("ask the service what it serves")?;
    let frame = read.pointed().context("read the frame of what the service serves")?;
    if frame.len() < FRAME_BYTES {
        bail!("the service answered a frame of {} bytes", frame.len());
    }
    let name = read.pointed().context("read the name of what the service serves")?;
    Ok(String::from_utf8_lossy(name.strip_suffix(&[0]).unwrap_or(name)).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn written(m: &Message) -> Vec<(u32, u64, Vec<u8>)> {
        m.addresses
            .iter()
            .map(|at| {
                let raw = &m.data[*at as usize..*at as usize + ADDRESS_BYTES];
                let flags = u32::from_le_bytes(raw[4..8].try_into().unwrap());
                let points = u64::from_le_bytes(raw[8..16].try_into().unwrap());
                let length = u64::from_le_bytes(raw[16..24].try_into().unwrap()) as usize;
                let inside = u64::from_le_bytes(raw[24..32].try_into().unwrap());
                let bytes = unsafe { std::slice::from_raw_parts(points as *const u8, length) }.to_vec();
                assert_eq!(u32::from_le_bytes(raw[0..4].try_into().unwrap()), ADDRESS);
                assert_eq!(u64::from_le_bytes(raw[32..40].try_into().unwrap()), 0);
                (flags, inside, bytes)
            })
            .collect()
    }

    #[test]
    fn a_call_opens_with_the_interface_name_and_a_terminator() {
        let m = Message::call("a.b@1.0::IC");
        assert_eq!(&m.data[..11], b"a.b@1.0::IC");
        assert_eq!(m.data.len(), 12, "the name, its terminator, and padding to four");
        assert_eq!(m.data[11], 0);
        assert!(m.addresses.is_empty());
    }

    #[test]
    fn numbers_are_written_where_they_fall() {
        let mut m = Message::call("a.b@1.0::IC");
        m.u32(7);
        m.u64(1 << 40);
        assert_eq!(m.data.len(), 12 + 4 + 8, "a word is not moved to a boundary of its own");
        assert_eq!(u32::from_le_bytes(m.data[12..16].try_into().unwrap()), 7);
        assert_eq!(u64::from_le_bytes(m.data[16..24].try_into().unwrap()), 1 << 40);
    }

    #[test]
    fn bytes_are_written_as_a_frame_and_the_bytes_it_points_at() {
        let mut m = Message::call("a.b@1.0::IC");
        m.bytes(&[1, 2, 3]);
        let parts = written(&m);
        assert_eq!(parts.len(), 2, "the frame, then the bytes");
        assert_eq!(parts[0].0, 0, "the frame points inside nothing");
        assert_eq!(parts[0].2.len(), FRAME_BYTES);
        assert_eq!(u32::from_le_bytes(parts[0].2[8..12].try_into().unwrap()), 3, "how many bytes there are");
        assert_eq!(parts[1].0, INSIDE_ANOTHER, "the bytes sit inside the frame");
        assert_eq!(parts[1].1, 0, "inside the first address of the call");
        assert_eq!(parts[1].2, vec![1, 2, 3]);
        assert_eq!(m.pointed, FRAME_BYTES as u64 + WORD, "both rounded up to a word");
    }

    #[test]
    fn text_counts_its_characters_and_carries_a_terminator() {
        let mut m = Message::call("a.b@1.0::IC");
        m.text("default");
        let parts = written(&m);
        assert_eq!(u32::from_le_bytes(parts[0].2[8..12].try_into().unwrap()), 7, "the terminator is not counted");
        assert_eq!(parts[1].2, b"default\0", "but it is carried");
    }

    #[test]
    fn the_frame_points_at_the_bytes_that_follow_it() {
        let mut m = Message::call("a.b@1.0::IC");
        m.bytes(&[9; 5]);
        let parts = written(&m);
        let in_frame = u64::from_le_bytes(parts[0].2[0..8].try_into().unwrap());
        let raw = &m.data[m.addresses[1] as usize..];
        assert_eq!(in_frame, u64::from_le_bytes(raw[8..16].try_into().unwrap()));
    }

    #[test]
    fn a_service_that_refused_the_call_is_an_error() {
        let mut reply = 3i32.to_le_bytes().to_vec();
        reply.extend_from_slice(&(-1i32).to_le_bytes());
        assert!(Answer::new(&reply).status().is_err());
        assert!(Answer::new(&0i32.to_le_bytes()).status().is_ok());
    }

    #[test]
    fn reading_past_the_end_of_a_reply_is_an_error() {
        assert!(Answer::new(&[]).i32().is_err());
        assert!(Answer::new(&[0; 8]).pointed().is_err());
    }

    #[test]
    fn a_truth_value_is_a_byte_padded_to_four() {
        let mut read = Answer::new(&[1, 0, 0, 0, 0, 0, 0, 0, 7, 0, 0, 0]);
        assert!(read.bool().unwrap());
        assert!(!read.bool().unwrap());
        assert_eq!(read.i32().unwrap(), 7, "what follows it starts four bytes on");
        assert!(Answer::new(&[1, 0]).bool().is_err());
    }
}
