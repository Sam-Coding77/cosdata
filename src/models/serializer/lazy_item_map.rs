use super::CustomSerialize;
use crate::models::buffered_io::{BufIoError, BufferManagerFactory};
use crate::models::cache_loader::{Cacheable, NodeRegistry};
use crate::models::identity_collections::{IdentityMap, IdentityMapKey};
use crate::models::lazy_load::{FileIndex, LazyItem, LazyItemMap, SyncPersist, CHUNK_SIZE};
use crate::models::types::FileOffset;
use crate::models::versioning::Hash;
use std::collections::HashSet;
use std::{io, sync::Arc};

const MSB: u32 = 1 << 31;

impl<T> CustomSerialize for LazyItemMap<T>
where
    T: Cacheable + Clone + CustomSerialize + 'static,
{
    fn serialize(
        &self,
        bufmans: Arc<BufferManagerFactory<Hash>>,
        version: Hash,
        cursor: u64,
    ) -> Result<u32, BufIoError> {
        if self.is_empty() {
            return Ok(u32::MAX);
        };
        let bufman = bufmans.get(version)?;
        let start_offset = bufman.cursor_position(cursor)? as u32;
        let mut items_arc = self.items.clone();
        let items: Vec<_> = items_arc
            .get()
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let total_items = items.len();

        for chunk_start in (0..total_items).step_by(CHUNK_SIZE) {
            let chunk_end = std::cmp::min(chunk_start + CHUNK_SIZE, total_items);
            let is_last_chunk = chunk_end == total_items;

            // Write placeholders for item offsets
            let placeholder_start = bufman.cursor_position(cursor)? as u32;
            for _ in 0..CHUNK_SIZE {
                bufman.update_u32_with_cursor(cursor, u32::MAX)?;
                bufman.update_u32_with_cursor(cursor, u32::MAX)?;
                bufman.update_u16_with_cursor(cursor, u16::MAX)?;
                bufman.update_u32_with_cursor(cursor, u32::MAX)?;
            }
            // Write placeholder for next chunk link
            let next_chunk_placeholder = bufman.cursor_position(cursor)? as u32;
            bufman.update_u32_with_cursor(cursor, u32::MAX)?;

            // Serialize items and update placeholders
            for i in chunk_start..chunk_end {
                let key_offset = items[i].0.serialize(bufmans.clone(), version, cursor)?;
                let item_offset = items[i].1.serialize(bufmans.clone(), version, cursor)?;

                let placeholder_pos = placeholder_start as u64 + ((i - chunk_start) as u64 * 14);
                let current_pos = bufman.cursor_position(cursor)?;

                // Write entry offset
                bufman.seek_with_cursor(cursor, placeholder_pos)?;
                bufman.update_u32_with_cursor(cursor, key_offset)?;
                bufman.update_u32_with_cursor(cursor, item_offset)?;
                bufman.update_u16_with_cursor(cursor, items[i].1.get_current_version_number())?;
                bufman.update_u32_with_cursor(cursor, *items[i].1.get_current_version())?;

                // Return to the current position
                bufman.seek_with_cursor(cursor, current_pos)?;
            }

            // Write next chunk link
            let next_chunk_start = bufman.cursor_position(cursor)? as u32;
            bufman.seek_with_cursor(cursor, next_chunk_placeholder as u64)?;
            if is_last_chunk {
                bufman.update_u32_with_cursor(cursor, u32::MAX)?; // Last chunk
            } else {
                bufman.update_u32_with_cursor(cursor, next_chunk_start)?;
            }
            bufman.seek_with_cursor(cursor, next_chunk_start as u64)?;
        }
        Ok(start_offset)
    }

    fn deserialize(
        bufmans: Arc<BufferManagerFactory<Hash>>,
        file_index: FileIndex,
        cache: Arc<NodeRegistry>,
        max_loads: u16,
        skipm: &mut HashSet<u64>,
    ) -> Result<Self, BufIoError> {
        match file_index {
            FileIndex::Invalid => Ok(LazyItemMap::new()),
            FileIndex::Valid {
                offset: FileOffset(offset),
                version_number,
                version_id,
            } => {
                if offset == u32::MAX {
                    return Ok(LazyItemMap::new());
                }
                let bufman = bufmans.get(version_id)?;
                let cursor = bufman.open_cursor()?;
                bufman.seek_with_cursor(cursor, offset as u64)?;
                let mut items = Vec::new();
                let mut current_chunk = offset;
                loop {
                    for i in 0..CHUNK_SIZE {
                        bufman.seek_with_cursor(cursor, current_chunk as u64 + (i as u64 * 14))?;
                        let key_offset = bufman.read_u32_with_cursor(cursor)?;
                        let item_offset = bufman.read_u32_with_cursor(cursor)?;
                        let item_version_number = bufman.read_u16_with_cursor(cursor)?;
                        let item_version_id = bufman.read_u32_with_cursor(cursor)?.into();
                        if key_offset == u32::MAX {
                            continue;
                        }
                        let key_file_index = FileIndex::Valid {
                            offset: FileOffset(key_offset),
                            version_number,
                            version_id,
                        };
                        let key = IdentityMapKey::deserialize(
                            bufmans.clone(),
                            key_file_index,
                            cache.clone(),
                            max_loads,
                            skipm,
                        )?;
                        let item_file_index = FileIndex::Valid {
                            offset: FileOffset(item_offset),
                            version_number: item_version_number,
                            version_id: item_version_id,
                        };
                        let item = LazyItem::deserialize(
                            bufmans.clone(),
                            item_file_index,
                            cache.clone(),
                            max_loads,
                            skipm,
                        )?;
                        items.push((key, item));
                    }
                    bufman
                        .seek_with_cursor(cursor, current_chunk as u64 + CHUNK_SIZE as u64 * 14)?;
                    // Read next chunk link
                    current_chunk = bufman.read_u32_with_cursor(cursor)?;
                    if current_chunk == u32::MAX {
                        break;
                    }
                }
                bufman.close_cursor(cursor)?;
                Ok(LazyItemMap::from_map(IdentityMap::from_iter(
                    items.into_iter(),
                )))
            }
        }
    }
}

impl CustomSerialize for IdentityMapKey {
    fn serialize(
        &self,
        bufmans: Arc<BufferManagerFactory<Hash>>,
        version: Hash,
        cursor: u64,
    ) -> Result<u32, BufIoError> {
        let bufman = bufmans.get(version)?;
        let start = bufman.cursor_position(cursor)? as u32;
        match self {
            Self::String(str) => {
                let bytes = str.clone().into_bytes();
                let len = bytes.len() as u32;
                bufman.update_u32_with_cursor(cursor, MSB | len)?;
                bufman.update_with_cursor(cursor, &bytes)?;
            }
            Self::Int(int) => {
                bufman.update_u32_with_cursor(cursor, *int)?;
            }
        }
        Ok(start)
    }
    fn deserialize(
        bufmans: Arc<BufferManagerFactory<Hash>>,
        file_index: FileIndex,
        _cache: Arc<NodeRegistry>,
        _max_loads: u16,
        _skipm: &mut HashSet<u64>,
    ) -> Result<Self, BufIoError>
    where
        Self: Sized,
    {
        match file_index {
            FileIndex::Valid {
                offset: FileOffset(offset),
                version_id,
                ..
            } => {
                let bufman = bufmans.get(version_id)?;
                let cursor = bufman.open_cursor()?;
                bufman.seek_with_cursor(cursor, offset as u64)?;
                let num = bufman.read_u32_with_cursor(cursor)?;
                if num & MSB == 0 {
                    return Ok(IdentityMapKey::Int(num));
                }
                // discard the most significant bit
                let len = (num << 1) >> 1;
                let mut bytes = vec![0; len as usize];
                bufman.read_with_cursor(cursor, &mut bytes)?;
                let str = String::from_utf8(bytes).map_err(|e| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("Invalid identity map key: {}", e),
                    )
                })?;
                bufman.close_cursor(cursor)?;
                Ok(IdentityMapKey::String(str))
            }
            FileIndex::Invalid => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "Cannot deserialize IdentityMapKey with an invalid FileIndex",
            )
            .into()),
        }
    }
}
