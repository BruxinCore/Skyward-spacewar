use crate::image;
use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use skyward_bootimg::BootImage;
use skyward_vab::Slot;
use std::fs::File;
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};

const PARTITION: &str = "recovery";
const BOOT_PARTITION: &str = "boot";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Held {
    pub digest: [u8; 32],
    pub skyward: bool,
}

pub struct Kept {
    dir: PathBuf,
    in_boot: bool,
    image: Vec<u8>,
    own: [u8; 32],
    before: [Held; 2],
}

fn target_partition(dir: &Path) -> (&'static str, bool) {
    if image::device(dir, PARTITION, Slot::A).exists() {
        (PARTITION, false)
    } else {
        (BOOT_PARTITION, true)
    }
}

impl Kept {
    pub fn hold(dir: &Path, current: Slot) -> Result<Option<Kept>> {
        let (part, in_boot) = target_partition(dir);
        let (a, image_a) = read(&image::device(dir, part, Slot::A), in_boot)?;
        let (b, image_b) = read(&image::device(dir, part, Slot::B), in_boot)?;
        let (held, image) = match current {
            Slot::A => (a, image_a),
            Slot::B => (b, image_b),
        };
        Ok(held.skyward.then(|| Kept { dir: dir.to_path_buf(), in_boot, image, own: held.digest, before: [a, b] }))
    }

    pub fn wanted(&self, next: Slot) -> Result<Vec<Slot>> {
        let part = if self.in_boot { BOOT_PARTITION } else { PARTITION };
        let (a, _) = read(&image::device(&self.dir, part, Slot::A), self.in_boot)?;
        let (b, _) = read(&image::device(&self.dir, part, Slot::B), self.in_boot)?;
        Ok(restore(self.before, [a, b], self.own, next))
    }

    pub fn write(&self, slots: &[Slot], progress: &mut dyn FnMut(u64, u64)) -> Result<()> {
        if !self.in_boot {
            let devices: Vec<PathBuf> = slots.iter().map(|&slot| image::device(&self.dir, PARTITION, slot)).collect();
            return image::flash(&mut Cursor::new(&self.image), self.image.len() as u64, &devices, progress);
        }
        let count = slots.len() as u64;
        for (idx, &slot) in slots.iter().enumerate() {
            let dev = image::device(&self.dir, BOOT_PARTITION, slot);
            let (_, target_bytes) = read(&dev, true)?;
            let patched = BootImage::patch_skyward_from(&target_bytes, &self.image)
                .with_context(|| format!("patch Skyward into {}", dev.display()))?;
            let len = patched.len() as u64;
            let step = idx as u64;
            image::flash(&mut Cursor::new(&patched), len, std::slice::from_ref(&dev), &mut |done, total| {
                progress(step * total + done, count * total);
            })?;
        }
        Ok(())
    }
}

fn read(path: &Path, in_boot: bool) -> Result<(Held, Vec<u8>)> {
    let mut file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    image::drop_cache(&file).with_context(|| format!("drop the cached pages of {}", path.display()))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).with_context(|| format!("read {}", path.display()))?;
    let skyward = image::is_skyward(&mut Cursor::new(&bytes)).with_context(|| format!("look into {}", path.display()))?;
    let digest = match (in_boot, skyward) {
        (true, true) => BootImage::skyward_digest(&bytes).unwrap_or_else(|| Sha256::digest(&bytes).into()),
        _ => Sha256::digest(&bytes).into(),
    };
    Ok((Held { digest, skyward }, bytes))
}

pub fn restore(before: [Held; 2], after: [Held; 2], own: [u8; 32], next: Slot) -> Vec<Slot> {
    [Slot::A, Slot::B]
        .into_iter()
        .filter(|&slot| {
            let k = slot.number() as usize;
            match slot == next {
                true => after[k].digest != own,
                false => after[k].digest != before[k].digest && !after[k].skyward,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::samples::{boot_image, lz4_legacy, recovery};
    use skyward_bootimg::Ramdisk;

    const SKYWARD: Held = Held { digest: [1; 32], skyward: true };
    const OWN: [u8; 32] = SKYWARD.digest;
    const OTHER_BUILD: Held = Held { digest: [4; 32], skyward: true };
    const STOCK: Held = Held { digest: [2; 32], skyward: false };
    const OLD: Held = Held { digest: [3; 32], skyward: false };

    #[test]
    fn the_slot_that_starts_next_gets_this_build() {
        assert_eq!(restore([SKYWARD, OLD], [SKYWARD, STOCK], OWN, Slot::B), [Slot::B], "an update or a package that went to slot B");
        assert_eq!(restore([SKYWARD, OLD], [SKYWARD, OLD], OWN, Slot::B), [Slot::B], "an older recovery there, left alone by the install");
        assert_eq!(restore([SKYWARD, OTHER_BUILD], [SKYWARD, OTHER_BUILD], OWN, Slot::B), [Slot::B], "another build of Skyward there");
        assert!(restore([SKYWARD, SKYWARD], [SKYWARD, SKYWARD], OWN, Slot::A).is_empty(), "this recovery is there already");
    }

    #[test]
    fn elsewhere_it_goes_back_only_where_the_install_wrote_something_else() {
        assert!(restore([SKYWARD, OLD], [SKYWARD, OLD], OWN, Slot::A).is_empty(), "a package that left recovery alone, such as a root installer");
        assert_eq!(restore([SKYWARD, OLD], [SKYWARD, STOCK], OWN, Slot::A), [Slot::B]);
        assert_eq!(restore([SKYWARD, OLD], [STOCK, STOCK], OWN, Slot::A), [Slot::A, Slot::B]);
        assert!(restore([SKYWARD, OLD], [SKYWARD, OTHER_BUILD], OWN, Slot::A).is_empty(), "a package that put another build of Skyward in");
    }

    fn partitions(a: &[u8], b: &[u8], size: usize) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for (name, image) in [("recovery_a", a), ("recovery_b", b)] {
            let mut bytes = image.to_vec();
            bytes.resize(size, 0);
            std::fs::write(dir.path().join(name), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn this_recovery_goes_back_where_an_install_wrote_over_it() {
        let (skyward, stock) = (recovery(true), recovery(false));
        let size = skyward.len().max(stock.len()) + 4096;
        let dir = partitions(&skyward, &stock, size);
        let kept = Kept::hold(dir.path(), Slot::A).unwrap().expect("slot A's partition holds Skyward");
        assert!(kept.wanted(Slot::A).unwrap().is_empty(), "nothing written over");
        let mut written = stock.clone();
        written.resize(size, 0);
        std::fs::write(dir.path().join("recovery_a"), &written).unwrap();
        assert_eq!(kept.wanted(Slot::A).unwrap(), [Slot::A], "slot B held the stock recovery already");
        let mut told = Vec::new();
        kept.write(&[Slot::A], &mut |done, total| told.push((done, total))).unwrap();
        assert_eq!(told.last(), Some(&(2 * size as u64, 2 * size as u64)), "written and read back");
        let mut whole = skyward.clone();
        whole.resize(size, 0);
        assert_eq!(std::fs::read(dir.path().join("recovery_a")).unwrap(), whole);
        assert!(kept.wanted(Slot::A).unwrap().is_empty());
        assert_eq!(kept.wanted(Slot::B).unwrap(), [Slot::B], "an install that has slot B start next");
    }

    #[test]
    fn boot_partition_preserves_custom_kernel_on_anykernel_and_rom_installs() {
        let mut sky_rd = Ramdisk { entries: Vec::new() };
        sky_rd.add("init", 0o750, b"init-a".to_vec()).unwrap();
        sky_rd.add("system/bin/recovery", 0o750, b"skyward-bin".to_vec()).unwrap();
        sky_rd.add("skyward/device.toml", 0o644, b"[device]\ncodename = \"spacewar\"\n".to_vec()).unwrap();
        let sky_boot = boot_image(4096, &lz4_legacy(&sky_rd.archive().unwrap()));

        let mut new_kernel_boot = BootImage::parse(&sky_boot).unwrap();
        new_kernel_boot.kernel = vec![0x99; 8192];
        let anykernel_boot = new_kernel_boot.to_bytes().unwrap();

        let mut rom_rd = Ramdisk { entries: Vec::new() };
        rom_rd.add("init", 0o750, b"init-rom".to_vec()).unwrap();
        rom_rd.add("first_stage_ramdisk/fstab.qcom", 0o644, b"rom-fstab".to_vec()).unwrap();
        rom_rd.add("system/bin/recovery", 0o750, b"stock-recovery".to_vec()).unwrap();
        let mut rom_boot_img = BootImage::parse(&boot_image(4096, &lz4_legacy(&rom_rd.archive().unwrap()))).unwrap();
        rom_boot_img.kernel = vec![0x77; 4096];
        let rom_boot = rom_boot_img.to_bytes().unwrap();

        let size = 64 << 10;
        let dir = tempfile::tempdir().unwrap();
        for (name, img) in [("boot_a", &sky_boot), ("boot_b", &rom_boot)] {
            let mut b = img.clone();
            b.resize(size, 0);
            std::fs::write(dir.path().join(name), b).unwrap();
        }

        let kept = Kept::hold(dir.path(), Slot::A).unwrap().expect("holds Skyward in boot_a");
        let mut ak = anykernel_boot.clone();
        ak.resize(size, 0);
        std::fs::write(dir.path().join("boot_a"), &ak).unwrap();
        assert!(kept.wanted(Slot::A).unwrap().is_empty(), "AnyKernel3 kernel change keeps Skyward intact");

        assert_eq!(kept.wanted(Slot::B).unwrap(), [Slot::B]);
        kept.write(&[Slot::B], &mut |_, _| {}).unwrap();
        let patched_b = BootImage::read(&dir.path().join("boot_b")).unwrap();
        assert_eq!(patched_b.kernel, vec![0x77; 4096], "ROM kernel in boot_b is preserved");
        let patched_rd = Ramdisk::decode(&patched_b.ramdisk).unwrap();
        assert_eq!(patched_rd.find("first_stage_ramdisk/fstab.qcom").unwrap().data, b"rom-fstab");
        assert_eq!(patched_rd.find("system/bin/recovery").unwrap().data, b"skyward-bin");
    }

    #[test]
    fn nothing_is_kept_when_this_recovery_was_started_without_being_flashed() {
        let dir = partitions(&recovery(false), &recovery(false), 1 << 20);
        assert!(Kept::hold(dir.path(), Slot::B).unwrap().is_none());
        let erased = partitions(&recovery(true), &[], 1 << 20);
        assert!(Kept::hold(erased.path(), Slot::B).unwrap().is_none(), "an erased partition");
        assert!(Kept::hold(erased.path(), Slot::A).unwrap().is_some());
    }
}
