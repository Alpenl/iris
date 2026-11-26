//! Split a complete upstream response without dropping its tail or remote cursor.
use iris_plugin_sdk::api::v2::{
    Id,
    source::{SourceEntry, SourcePage},
};
use std::collections::{BTreeMap, VecDeque};

#[derive(PartialEq, Eq)]
pub(crate) struct PageTarget {
    pub account_epoch: u64,
    pub source: String,
    pub folder: Option<String>,
    pub query: Option<String>,
    pub filters: BTreeMap<String, String>,
}

pub(crate) struct PageCache {
    target: PageTarget,
    serial: Id,
    requires_auth: bool,
    offset: usize,
    page: SourcePage,
    remaining: VecDeque<SourceEntry>,
}

impl PageCache {
    pub fn new(target: PageTarget, serial: Id, requires_auth: bool, mut page: SourcePage) -> Self {
        let remaining = std::mem::take(&mut page.entries).into();
        Self {
            target,
            serial,
            requires_auth,
            offset: 0,
            page,
            remaining,
        }
    }

    pub fn requires_auth(&self) -> bool {
        self.requires_auth
    }

    fn cursor(&self) -> String {
        format!("local:{}:{}", self.serial.0, self.offset)
    }

    pub fn matches(&self, target: &PageTarget, cursor: &str) -> bool {
        self.has_more() && &self.target == target && cursor == self.cursor()
    }

    pub fn has_more(&self) -> bool {
        !self.remaining.is_empty()
    }

    pub fn next(&mut self) -> Result<SourcePage, String> {
        let mut page = self.page.clone();
        let mut bytes = 0;
        while let Some(entry) = self.remaining.front() {
            let len = serde_json::to_vec(entry).map_err(|e| e.to_string())?.len() + 1;
            if bytes + len > 42 * 1024 || page.entries.len() >= 60 {
                break;
            }
            bytes += len;
            page.entries.push(self.remaining.pop_front().unwrap());
        }
        if page.entries.is_empty() && self.has_more() {
            return Err("目录条目超过单页大小限制".into());
        }
        self.offset += page.entries.len();
        if self.has_more() {
            page.next_page = Some(self.cursor());
        }
        // On the last local page, keep the upstream continuation from metadata.
        Ok(page)
    }
}
