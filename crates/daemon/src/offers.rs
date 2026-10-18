//! Folders other devices offer us: syncthing's "device X wants to share
//! folder Y". Kept in memory and outliving engine restarts (the run loop owns
//! it); the offering device repeats the offer every pull interval anyway.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;
use vuoxi_core::PeerId;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub folder: String,
    pub from: PeerId,
    pub tag: String,
    pub key: Option<[u8; 32]>,
    pub members: Vec<PeerId>,
}

#[derive(Default)]
pub struct Offers {
    pending: Mutex<HashMap<(String, PeerId), Offer>>,
    dismissed: Mutex<HashSet<(String, PeerId)>>,
}

impl Offers {
    pub fn record(&self, o: Offer) {
        let k = (o.folder.clone(), o.from);
        if !self.dismissed.lock().unwrap().contains(&k) {
            self.pending.lock().unwrap().insert(k, o);
        }
    }

    pub fn list(&self) -> Vec<Offer> {
        let mut v: Vec<Offer> = self.pending.lock().unwrap().values().cloned().collect();
        v.sort_by(|a, b| a.folder.cmp(&b.folder));
        v
    }

    pub fn get(&self, folder: &str, from: PeerId) -> Option<Offer> {
        self.pending
            .lock()
            .unwrap()
            .get(&(folder.to_owned(), from))
            .cloned()
    }

    /// Accepted: drop every offer of that folder.
    pub fn take(&self, folder: &str) {
        self.pending.lock().unwrap().retain(|(f, _), _| f != folder);
    }

    pub fn dismiss(&self, folder: &str, from: PeerId) {
        let k = (folder.to_owned(), from);
        self.pending.lock().unwrap().remove(&k);
        self.dismissed.lock().unwrap().insert(k);
    }
}
