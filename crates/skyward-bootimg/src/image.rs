use crate::Ramdisk;
use abootimg_oxide::{binrw::io::Cursor, Header, HeaderV3};
use anyhow::{anyhow, bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

const PAGE: usize = 4096;
const FOOTER_LEN: usize = 64;
const HEADER_LEN: usize = 256;
const TAG_HASH: u64 = 2;
const RELEASE: &[u8] = b"skyward-pack";

pub struct BootImage {
    pub header: HeaderV3,
    pub kernel: Vec<u8>,
    pub ramdisk: Vec<u8>,
}

fn section(bytes: &[u8], at: usize, len: u32, what: &str) -> Result<Vec<u8>> {
    let end = at + len as usize;
    bytes.get(at..end).map(<[u8]>::to_vec).ok_or_else(|| anyhow!("{what} runs past the end of the image"))
}

impl BootImage {
    pub fn read(path: &Path) -> Result<Self> {
        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        BootImage::parse(&bytes).with_context(|| format!("read {}", path.display()))
    }

    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let header = match Header::parse(&mut Cursor::new(bytes)).context("header")? {
            Header::V3(h) => h,
            Header::V0(h) => bail!("header version {} predates the ramdisk-only layout", h.header_version()),
        };
        let kernel = section(bytes, HeaderV3::kernel_position(), header.kernel_size, "kernel")?;
        let ramdisk = section(bytes, header.ramdisk_position(), header.ramdisk_size, "ramdisk")?;
        Ok(BootImage { header, kernel, ramdisk })
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut header = self.header.clone();
        header.kernel_size = u32::try_from(self.kernel.len()).context("kernel size")?;
        header.ramdisk_size = u32::try_from(self.ramdisk.len()).context("ramdisk size")?;
        header.v4_signature_size = header.v4_signature_size.map(|_| 0);
        let mut out = Vec::new();
        Header::V3(header).write(&mut out).context("write header")?;
        pad(&mut out);
        out.extend_from_slice(&self.kernel);
        pad(&mut out);
        out.extend_from_slice(&self.ramdisk);
        pad(&mut out);
        Ok(out)
    }

    pub fn patch_skyward_from(target_bytes: &[u8], skyward_bytes: &[u8]) -> Result<Vec<u8>> {
        let mut target = BootImage::parse(target_bytes).context("parse target boot image")?;
        let source = BootImage::parse(skyward_bytes).context("parse source skyward image")?;
        let mut target_rd = Ramdisk::decode(&target.ramdisk).context("decode target ramdisk")?;
        let source_rd = Ramdisk::decode(&source.ramdisk).context("decode source skyward ramdisk")?;
        target_rd.transplant_skyward_from(&source_rd)?;
        target.ramdisk = target_rd.encode_in_process_matching(&target.ramdisk)?;
        let raw = target.to_bytes()?;
        if target_bytes.len() >= raw.len() + PAGE * 2 {
            let part_name = avb_partition_name(target_bytes).unwrap_or("boot");
            let salt = Sha256::digest(&raw);
            return avb_footed(&raw, part_name, target_bytes.len(), &salt);
        }
        Ok(raw)
    }

    pub fn skyward_digest(bytes: &[u8]) -> Option<[u8; 32]> {
        let img = BootImage::parse(bytes).ok()?;
        if img.ramdisk.is_empty() {
            return None;
        }
        let rd = Ramdisk::decode(&img.ramdisk).ok()?;
        rd.skyward_digest()
    }
}

pub fn avb_partition_name(bytes: &[u8]) -> Option<&'static str> {
    if bytes.len() < FOOTER_LEN {
        return None;
    }
    let footer = &bytes[bytes.len() - FOOTER_LEN..];
    if &footer[..4] != b"AVBf" {
        return None;
    }
    let vbm_off = u64::from_be_bytes(footer[20..28].try_into().ok()?) as usize;
    let vbm_sz = u64::from_be_bytes(footer[28..36].try_into().ok()?) as usize;
    let vbm = bytes.get(vbm_off..vbm_off.checked_add(vbm_sz)?)?;
    if vbm.windows(4).any(|w| w == b"boot") {
        Some("boot")
    } else if vbm.windows(8).any(|w| w == b"recovery") {
        Some("recovery")
    } else {
        None
    }
}

pub fn avb_footed(image: &[u8], partition: &str, partition_size: usize, salt: &[u8]) -> Result<Vec<u8>> {
    let digest = Sha256::new().chain_update(salt).chain_update(image).finalize();
    let vbmeta = vbmeta(&hash_descriptor(image.len() as u64, partition, salt, &digest));
    let offset = image.len().next_multiple_of(PAGE);
    let end = offset + vbmeta.len().next_multiple_of(PAGE);
    ensure!(
        end + PAGE <= partition_size,
        "a {}-byte image and its vbmeta image do not fit a {partition_size}-byte {partition} partition beside its footer",
        image.len()
    );
    let mut out = Vec::with_capacity(partition_size);
    out.extend_from_slice(image);
    out.resize(offset, 0);
    out.extend_from_slice(&vbmeta);
    out.resize(partition_size - FOOTER_LEN, 0);
    out.extend_from_slice(b"AVBf");
    out.extend_from_slice(&1u32.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes());
    for n in [image.len(), offset, vbmeta.len()] {
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.resize(partition_size, 0);
    Ok(out)
}

fn hash_descriptor(image_size: u64, partition: &str, salt: &[u8], digest: &[u8]) -> Vec<u8> {
    let mut algorithm = [0u8; 32];
    algorithm[..6].copy_from_slice(b"sha256");
    let mut d = Vec::new();
    d.extend_from_slice(&TAG_HASH.to_be_bytes());
    d.extend_from_slice(&0u64.to_be_bytes());
    d.extend_from_slice(&image_size.to_be_bytes());
    d.extend_from_slice(&algorithm);
    for n in [partition.len(), salt.len(), digest.len(), 0] {
        d.extend_from_slice(&(n as u32).to_be_bytes());
    }
    d.extend_from_slice(&[0; 60]);
    d.extend_from_slice(partition.as_bytes());
    d.extend_from_slice(salt);
    d.extend_from_slice(digest);
    d.resize(d.len().next_multiple_of(8), 0);
    let following = (d.len() - 16) as u64;
    d[8..16].copy_from_slice(&following.to_be_bytes());
    d
}

fn vbmeta(descriptors: &[u8]) -> Vec<u8> {
    let aux = descriptors.len().next_multiple_of(64);
    let mut v = Vec::with_capacity(HEADER_LEN + aux);
    v.extend_from_slice(b"AVB0");
    v.extend_from_slice(&1u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u64.to_be_bytes());
    v.extend_from_slice(&(aux as u64).to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    let n = descriptors.len();
    for field in [0, 0, 0, 0, n, 0, n, 0, 0, n, 0] {
        v.extend_from_slice(&(field as u64).to_be_bytes());
    }
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(&0u32.to_be_bytes());
    v.extend_from_slice(RELEASE);
    v.resize(HEADER_LEN, 0);
    v.extend_from_slice(descriptors);
    v.resize(HEADER_LEN + aux, 0);
    v
}

fn pad(out: &mut Vec<u8>) {
    out.resize(out.len().div_ceil(PAGE) * PAGE, 0);
}
