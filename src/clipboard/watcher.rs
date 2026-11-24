use super::native::{read_snapshot, sequence, write, write_if_current};
use crate::error::{Error, Result};
use zeroize::Zeroizing;

#[derive(Default)]
pub struct Watcher {
    sent: Option<u32>,
    remote: Option<u32>,
}

impl Watcher {
    pub fn paste(&self, owner: isize) -> Result<Option<(u32, Zeroizing<String>)>> {
        let sequence = sequence();
        if self.remote == Some(sequence) || self.sent == Some(sequence) {
            return Ok(None);
        }
        read_snapshot(owner)?.map(Some).ok_or_else(|| {
            Error::Clipboard(
                "Only plain-text clipboard sharing is supported; nothing was pasted".into(),
            )
        })
    }

    pub fn sent(&mut self, sequence: u32) {
        self.sent = Some(sequence);
    }

    pub fn receive_if_current(
        &mut self,
        owner: isize,
        text: &str,
        expected: u32,
    ) -> Result<Option<u32>> {
        let applied = write_if_current(owner, text, Some(expected))?;
        if applied.is_some() {
            self.remote = applied;
        }
        Ok(applied)
    }

    pub fn receive(&mut self, owner: isize, text: &str) -> Result<()> {
        write(owner, text)?;
        let sequence = sequence();
        self.remote = Some(sequence);
        Ok(())
    }
}
