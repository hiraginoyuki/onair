use parking_lot::Mutex;

use super::records::InspectorRequestRecord;
use super::store::InspectorStore;

pub struct LiveRecord {
    store: InspectorStore,
    enabled: bool,
    retention_requests: usize,
    record: Mutex<InspectorRequestRecord>,
}

impl LiveRecord {
    pub fn new(
        store: InspectorStore,
        enabled: bool,
        retention_requests: usize,
        initial: InspectorRequestRecord,
    ) -> Self {
        Self {
            store,
            enabled,
            retention_requests,
            record: Mutex::new(initial),
        }
    }

    pub fn publish_initial(&self) {
        if !self.enabled {
            return;
        }
        let record = self.record.lock().clone();
        self.store
            .upsert(self.enabled, self.retention_requests, record);
    }

    pub fn update<F>(&self, mutate: F)
    where
        F: FnOnce(&mut InspectorRequestRecord),
    {
        if !self.enabled {
            return;
        }
        let snapshot = {
            let mut record = self.record.lock();
            mutate(&mut record);
            record.clone()
        };
        self.store
            .upsert(self.enabled, self.retention_requests, snapshot);
    }

    #[allow(dead_code)]
    pub fn snapshot(&self) -> InspectorRequestRecord {
        self.record.lock().clone()
    }

    pub fn finalize(self, mut final_record: InspectorRequestRecord) {
        if !self.enabled {
            return;
        }
        let started = self.record.lock().base.started_at_unix_ms;
        final_record.base.started_at_unix_ms = started;
        final_record.timeline.started_unix_ms = started;
        self.store
            .upsert_final(self.enabled, self.retention_requests, final_record);
    }
}
