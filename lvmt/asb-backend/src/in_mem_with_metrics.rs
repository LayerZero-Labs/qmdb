#![allow(unused)]

// The original journaldb relies on some metric run in crate `ethcore-db`. But it doesn't rely on the other dependencies.

// Copyright 2015-2020 Parity Technologies (UK) Ltd.
// The following code is part of OpenEthereum.

// OpenEthereum is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// OpenEthereum is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with OpenEthereum.  If not, see <http://www.gnu.org/licenses/>.

use kvdb::KeyValueDB;
use std::ops::Deref;

/// InMemory with disabled statistics
pub struct InMemoryWithMetrics {
    db: kvdb_memorydb::InMemory,
}

impl Deref for InMemoryWithMetrics {
    type Target = kvdb_memorydb::InMemory;

    fn deref(&self) -> &Self::Target {
        &self.db
    }
}

impl stats::PrometheusMetrics for InMemoryWithMetrics {
    fn prometheus_metrics(&self, _: &mut stats::PrometheusRegistry) {}
}

impl InMemoryWithMetrics {
    /// Create new instance
    pub fn create(num_cols: u32) -> Self {
        Self {
            db: kvdb_memorydb::create(num_cols),
        }
    }
}
