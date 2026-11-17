use anyhow::Result;
use skyward_binder::driver::Binder;
use skyward_crypto::data::{self, Closed, Opening};
use skyward_crypto::keymint::KeyMint;
use skyward_sys::worker::Worker;
use std::os::fd::RawFd;

pub enum Job {
    Open,
    Unlock { closed: Vec<Closed>, user: u32, credential: Vec<u8> },
}

pub enum Ended {
    Opened(Result<Opening, String>),
    Unlocked(Result<Vec<Closed>, String>),
}

pub struct Decryptor {
    worker: Worker<Ended>,
}

impl Decryptor {
    pub fn start(job: Job) -> Result<Decryptor> {
        let worker = Worker::start("decrypt", move |tell| {
            tell.tell(match job {
                Job::Open => Ended::Opened(with_key_service(data::open_without_credential)),
                Job::Unlock { closed, user, credential } => {
                    Ended::Unlocked(with_key_service(|service| data::unlock(service, &closed, user, &credential)))
                }
            });
        })?;
        Ok(Decryptor { worker })
    }

    pub fn fd(&self) -> RawFd {
        self.worker.fd()
    }

    pub fn ended(&self) -> Option<Ended> {
        self.worker.events().pop()
    }
}

pub fn with_key_service<T>(work: impl FnOnce(&KeyMint) -> Result<T>) -> Result<T, String> {
    let done = (|| {
        let needs_retry = match (Binder::open(), Binder::hardware()) {
            (Ok(b), Ok(h)) => KeyMint::open(&b, &h).is_err(),
            _ => true,
        };
        if needs_retry {
            crate::retry_key_services();
        }
        let (binder, hardware) = (Binder::open()?, Binder::hardware()?);
        let service = KeyMint::open(&binder, &hardware)?;
        work(&service)
    })();
    done.map_err(|e| format!("{e:#}"))
}
