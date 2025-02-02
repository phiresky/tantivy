use super::decompress;
use super::index::SkipIndex;
use crate::common::VInt;
use crate::common::{BinarySerializable, HasLen};
use crate::directory::{FileSlice, OwnedBytes};
use crate::schema::Document;
use crate::space_usage::StoreSpaceUsage;
use crate::store::index::Checkpoint;
use crate::DocId;
use lru::LruCache;
use tantivy_fst::Ulen;
use std::io;
use std::mem::size_of;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

const LRU_CACHE_CAPACITY: Ulen = 100;

type Block = Arc<Vec<u8>>;

type BlockCache = Arc<Mutex<LruCache<u64, Block>>>;

/// Reads document off tantivy's [`Store`](./index.html)
pub struct StoreReader {
    data: FileSlice,
    cache: BlockCache,
    cache_hits: Arc<AtomicUsize>,
    cache_misses: Arc<AtomicUsize>,
    skip_index: Arc<SkipIndex>,
    space_usage: StoreSpaceUsage,
}

impl StoreReader {
    /// Opens a store reader
    pub fn open(store_file: FileSlice) -> io::Result<StoreReader> {
        let (data_file, offset_index_file) = split_file(store_file)?;
        let index_data = offset_index_file.read_bytes()?;
        let space_usage = StoreSpaceUsage::new(data_file.len(), offset_index_file.len());
        let skip_index = SkipIndex::open(index_data);
        Ok(StoreReader {
            data: data_file,
            cache: Arc::new(Mutex::new(LruCache::new(LRU_CACHE_CAPACITY as usize))),
            cache_hits: Default::default(),
            cache_misses: Default::default(),
            skip_index: Arc::new(skip_index),
            space_usage,
        })
    }

    pub(crate) fn block_checkpoints(&self) -> impl Iterator<Item = Checkpoint> + '_ {
        self.skip_index.checkpoints()
    }

    fn block_checkpoint(&self, doc_id: DocId) -> Option<Checkpoint> {
        self.skip_index.seek(doc_id)
    }

    pub(crate) fn block_data(&self) -> io::Result<OwnedBytes> {
        self.data.read_bytes()
    }

    fn compressed_block(&self, checkpoint: &Checkpoint) -> io::Result<OwnedBytes> {
        self.data
            .slice(
                checkpoint.start_offset as Ulen,
                checkpoint.end_offset as Ulen,
            )
            .read_bytes()
    }

    fn read_block(&self, checkpoint: &Checkpoint) -> io::Result<Block> {
        if let Some(block) = self.cache.lock().unwrap().get(&checkpoint.start_offset) {
            self.cache_hits.fetch_add(1, Ordering::SeqCst);
            return Ok(block.clone());
        }

        self.cache_misses.fetch_add(1, Ordering::SeqCst);

        let compressed_block = self.compressed_block(checkpoint)?;
        let mut decompressed_block = vec![];
        decompress(compressed_block.as_slice(), &mut decompressed_block)?;

        let block = Arc::new(decompressed_block);
        self.cache
            .lock()
            .unwrap()
            .put(checkpoint.start_offset, block.clone());

        Ok(block)
    }

    fn cache_blocks_multiple(&self, checkpoints: &[Checkpoint]) -> io::Result<()> {
        // just to cache them so the next read is instant, TODO: don't rely on caching  within FileSlice, use self.cache instead?
        // crate::info_log("caching multiple");
        let ranges = checkpoints.iter().map(|c| (c.start_offset as Ulen)..(c.end_offset as Ulen)).collect::<Vec<_>>();
        self.data.read_bytes_slice_multiple(&ranges)?;
        // crate::info_log("caching multiple done");
        Ok(())
    }

    /// Reads a given document.
    ///
    /// Calling `.get(doc)` is relatively costly as it requires
    /// decompressing a compressed block.
    ///
    /// It should not be called to score documents
    /// for instance.
    pub fn get(&self, doc_id: DocId) -> crate::Result<Document> {
        let checkpoint = self.block_checkpoint(doc_id).ok_or_else(|| {
            crate::TantivyError::InvalidArgument(format!("Failed to lookup Doc #{}.", doc_id))
        })?;
        crate::info_log(format!("decompressing block for doc {}", doc_id));
        let mut cursor = &self.read_block(&checkpoint)?[..];
        for _ in checkpoint.start_doc..doc_id {
            let doc_length = VInt::deserialize(&mut cursor)?.val() as usize;
            cursor = &cursor[doc_length..];
        }

        let doc_length = VInt::deserialize(&mut cursor)?.val() as usize;
        cursor = &cursor[..doc_length];
        Ok(Document::deserialize(&mut cursor)?)
    }

    /// Reads the given document ids.
    /// May be faster than getting them separately if the storage backend supports it
    pub fn get_multiple(&self, doc_ids: &[DocId]) -> crate::Result<Vec<Document>> {
        let checkpoints: Vec<Checkpoint> = doc_ids.iter().flat_map(|doc_id| self.block_checkpoint(*doc_id)).collect();
        self.cache_blocks_multiple(&checkpoints)?;
        doc_ids.iter().map(|d| self.get(*d)).collect()
    }

    /// Summarize total space usage of this store reader.
    pub fn space_usage(&self) -> StoreSpaceUsage {
        self.space_usage.clone()
    }
}

fn split_file(data: FileSlice) -> io::Result<(FileSlice, FileSlice)> {
    let (data, footer_len_bytes) = data.split_from_end(size_of::<u64>() as Ulen);
    let serialized_offset: OwnedBytes = footer_len_bytes.read_bytes()?;
    let mut serialized_offset_buf = serialized_offset.as_slice();
    let offset = u64::deserialize(&mut serialized_offset_buf)? as Ulen;
    Ok(data.split(offset))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schema::Document;
    use crate::schema::Field;
    use crate::{directory::RAMDirectory, store::tests::write_lorem_ipsum_store, Directory};
    use std::path::Path;

    fn get_text_field<'a>(doc: &'a Document, field: &'a Field) -> Option<&'a str> {
        doc.get_first(*field).and_then(|f| f.text())
    }

    #[test]
    fn test_store_lru_cache() -> crate::Result<()> {
        let directory = RAMDirectory::create();
        let path = Path::new("store");
        let writer = directory.open_write(path)?;
        let schema = write_lorem_ipsum_store(writer, 500);
        let title = schema.get_field("title").unwrap();
        let store_file = directory.open_read(path)?;
        let store = StoreReader::open(store_file)?;

        assert_eq!(store.cache.lock().unwrap().len(), 0);
        assert_eq!(store.cache_hits.load(Ordering::SeqCst), 0);
        assert_eq!(store.cache_misses.load(Ordering::SeqCst), 0);

        let doc = store.get(0)?;
        assert_eq!(get_text_field(&doc, &title), Some("Doc 0"));

        assert_eq!(store.cache.lock().unwrap().len(), 1);
        assert_eq!(store.cache_hits.load(Ordering::SeqCst), 0);
        assert_eq!(store.cache_misses.load(Ordering::SeqCst), 1);
        assert_eq!(
            store
                .cache
                .lock()
                .unwrap()
                .peek_lru()
                .map(|(&k, _)| k as Ulen),
            Some(0)
        );

        let doc = store.get(499)?;
        assert_eq!(get_text_field(&doc, &title), Some("Doc 499"));

        assert_eq!(store.cache.lock().unwrap().len(), 2);
        assert_eq!(store.cache_hits.load(Ordering::SeqCst), 0);
        assert_eq!(store.cache_misses.load(Ordering::SeqCst), 2);

        assert_eq!(
            store
                .cache
                .lock()
                .unwrap()
                .peek_lru()
                .map(|(&k, _)| k as Ulen),
            Some(0)
        );

        let doc = store.get(0)?;
        assert_eq!(get_text_field(&doc, &title), Some("Doc 0"));

        assert_eq!(store.cache.lock().unwrap().len(), 2);
        assert_eq!(store.cache_hits.load(Ordering::SeqCst), 1);
        assert_eq!(store.cache_misses.load(Ordering::SeqCst), 2);
        assert_eq!(
            store
                .cache
                .lock()
                .unwrap()
                .peek_lru()
                .map(|(&k, _)| k as Ulen),
            Some(18806)
        );

        Ok(())
    }
}
