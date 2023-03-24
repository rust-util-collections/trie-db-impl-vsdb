#![deny(warnings)]
#![allow(clippy::new_without_default)]

use backend::{
    sp_trie_db::{CError, DBValue, HashDB, TrieItem, TrieIterator, TrieKeyItem},
    KeccakHasher as H, TrieBackend,
};
use ruc::*;
use sp_trie::{
    cache::{LocalTrieCache, TrieCache},
    trie_types::{TrieDB, TrieDBMutBuilderV1 as TrieDBMutBuilder, TrieDBMutV1 as TrieDBMut},
    LayoutV1, Trie, TrieDBBuilder, TrieHash, TrieMut,
};
use std::mem;
use vsdb::basic::mapx_ord_rawkey::MapxOrdRawKey;

type L = LayoutV1<H>;
pub type TrieRoot = TrieHash<L>;

pub type TrieIter<'a> = Box<dyn TrieIterator<L, Item = TrieItem<TrieHash<L>, CError<L>>> + 'a>;
pub type TrieKeyIter<'a> =
    Box<dyn TrieIterator<L, Item = TrieKeyItem<TrieHash<L>, CError<L>>> + 'a>;

#[derive(serde::Serialize, serde::Deserialize)]
pub struct MptStore {
    // backend key ==> backend instance
    //
    // the backend key
    // - for the world state MPT, it is `[0]`
    // - for the storage MPT, it is the bytes of a H160 address
    meta: MapxOrdRawKey<TrieBackend>,
}

impl MptStore {
    pub fn new() -> Self {
        Self {
            meta: MapxOrdRawKey::new(),
        }
    }

    pub fn trie_remove(&self, backend_key: &[u8]) {
        self.remove_backend(backend_key);
    }

    pub fn trie_create<'a>(
        &self,
        backend_key: &'a [u8],
        cache_size: Option<usize>,
        reset: bool,
    ) -> Result<MptOnce<'a>> {
        let backend = MptStore::new_backend(cache_size);
        self.put_backend(backend_key, &backend, reset).c(d!())?;

        let backend = Box::into_raw(Box::new(backend));
        unsafe {
            Ok(MptOnce {
                mpt: MptMut::new(&mut *backend),
                backend: Box::from_raw(backend),
            })
        }
    }

    /// @param cache_size:
    ///     - None, do nothing
    ///     - Some(negative value), close cache
    ///     - Some(0), reset the cache capacity to the default size
    ///     - Some(new_size), reset the cache capacity to the new size
    pub fn trie_restore<'a>(
        &self,
        backend_key: &'a [u8],
        cache_size: Option<isize>,
        root: TrieRoot,
    ) -> Result<MptOnce<'a>> {
        let mut backend = self.get_backend(backend_key).c(d!("backend not found"))?;
        if let Some(n) = cache_size {
            if 0 > n {
                backend.reset_cache(None);
            } else {
                backend.reset_cache(Some(n as usize));
            }
        }

        let backend = Box::into_raw(Box::new(backend));
        unsafe {
            Ok(MptOnce {
                mpt: MptMut::from_existing(&mut *backend, root),
                backend: Box::from_raw(backend),
            })
        }
    }

    fn get_backend(&self, backend_key: &[u8]) -> Option<TrieBackend> {
        self.meta.get(backend_key)
    }

    fn put_backend(&self, backend_key: &[u8], backend: &TrieBackend, reset: bool) -> Result<()> {
        let mut hdr = unsafe { self.meta.shadow() };

        if reset {
            hdr.remove(backend_key);
        } else if hdr.contains_key(backend_key) {
            return Err(eg!("backend key already exists"));
        }

        hdr.insert(backend_key, backend);

        Ok(())
    }

    fn remove_backend(&self, backend_key: &[u8]) {
        unsafe { self.meta.shadow() }.remove(backend_key);
    }

    fn new_backend(cache_size: Option<usize>) -> TrieBackend {
        TrieBackend::new(cache_size)
    }
}

///
/// # NOTE
///
/// The referenced field **MUST** be placed after the field that references it,
/// this is to ensure that the `drop`s can be executed in the correct order,
/// so that UB will not occur
pub struct MptOnce<'a> {
    mpt: MptMut<'a>,

    // self-reference
    #[allow(dead_code)]
    backend: Box<TrieBackend>,
}

impl<'a> MptOnce<'a> {
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.mpt.get(key).c(d!())
    }

    pub fn contains(&self, key: &[u8]) -> Result<bool> {
        self.mpt.contains(key).c(d!())
    }

    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.mpt.insert(key, value).c(d!())
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<()> {
        self.mpt.remove(key).c(d!()).map(|_| ())
    }

    pub fn clear(&mut self) -> Result<()> {
        self.mpt.clear().c(d!())
    }

    pub fn is_empty(&self) -> bool {
        self.mpt.is_empty()
    }

    pub fn commit(&mut self) -> TrieRoot {
        self.mpt.commit()
    }

    pub fn ro_handle(&self, root: TrieHash<L>) -> MptRo {
        MptRo::from_existing(&self.backend, root)
    }
}

///
/// # NOTE
///
/// The referenced field **MUST** be placed after the field that references it,
/// this is to ensure that the `drop`s can be executed in the correct order,
/// so that UB will not occur
pub struct MptMut<'a> {
    trie: TrieDBMut<'a, H>,

    // self-reference
    #[allow(dead_code)]
    meta: MptMeta<'a>,
}

impl<'a> MptMut<'a> {
    // keep private !!
    pub fn new(backend: &'a mut TrieBackend) -> Self {
        let lc = backend.get_cache_hdr().map(|hdr| hdr.local_cache());

        // The buf will be rewrited when building the target `Trie`,
        // so its original contents can be arbitrary values.
        let root_buf = Default::default();

        let meta = MptMeta::new(lc, root_buf, true);

        let trie = TrieDBMutBuilder::new(backend, unsafe { &mut *meta.root })
            .with_optional_cache(
                meta.cache
                    .as_ref()
                    .map(|c| unsafe { &mut *c.cache } as &mut dyn sp_trie::TrieCache<_>),
            )
            .build();

        Self { trie, meta }
    }

    pub fn from_existing(backend: &'a mut TrieBackend, root: TrieRoot) -> Self {
        let lc = backend.get_cache_hdr().map(|hdr| hdr.local_cache());
        let meta = MptMeta::new(lc, root, true);

        let trie = TrieDBMutBuilder::from_existing(backend, unsafe { &mut *meta.root })
            .with_optional_cache(
                meta.cache
                    .as_ref()
                    .map(|c| unsafe { &mut *c.cache } as &mut dyn sp_trie::TrieCache<_>),
            )
            .build();

        Self { trie, meta }
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.trie.get(key).c(d!())
    }

    pub fn contains(&self, key: &[u8]) -> Result<bool> {
        self.trie.contains(key).c(d!())
    }

    pub fn insert(&mut self, key: &[u8], value: &[u8]) -> Result<()> {
        self.trie.insert(key, value).c(d!()).map(|_| ())
    }

    pub fn remove(&mut self, key: &[u8]) -> Result<()> {
        self.trie.remove(key).c(d!()).map(|_| ())
    }

    pub fn clear(&mut self) -> Result<()> {
        let root = self.commit();
        let keys = self.ro_handle(root).key_iter().collect::<Vec<_>>();
        for k in keys.iter().map(|k| k.as_ref().unwrap()) {
            self.remove(k).c(d!())?;
        }
        Ok(())
    }

    pub fn is_empty(&self) -> bool {
        self.trie.is_empty()
    }

    pub fn commit(&mut self) -> TrieRoot {
        *self.trie.root()
    }

    pub fn ro_handle(&self, root: TrieHash<L>) -> MptRo {
        MptRo::from_existing_dyn(self.trie.db(), root)
    }
}

///
/// # NOTE
///
/// The referenced field **MUST** be placed after the field that references it,
/// this is to ensure that the `drop`s can be executed in the correct order,
/// so that UB will not occur
pub struct MptRo<'a> {
    trie: TrieDB<'a, 'a, H>,

    // self-reference
    #[allow(dead_code)]
    meta: MptMeta<'a>,
}

impl<'a> MptRo<'a> {
    pub fn from_existing(backend: &'a TrieBackend, root: TrieRoot) -> Self {
        let lc = backend.get_cache_hdr().map(|hdr| hdr.local_cache());
        let meta = MptMeta::new(lc, root, false);

        let trie = TrieDBBuilder::new(backend, unsafe { &*meta.root })
            .with_optional_cache(
                meta.cache
                    .as_ref()
                    .map(|c| unsafe { &mut *c.cache } as &mut dyn sp_trie::TrieCache<_>),
            )
            .build();

        Self { trie, meta }
    }

    pub fn from_existing_dyn(backend: &dyn HashDB<H, DBValue>, root: TrieRoot) -> Self {
        let backend = &backend as *const &dyn HashDB<H, DBValue>;
        let backend = backend.cast::<&TrieBackend>();
        let backend = unsafe { *backend };
        MptRo::from_existing(backend, root)
    }

    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.trie.get(key).c(d!())
    }

    pub fn contains(&self, key: &[u8]) -> Result<bool> {
        self.trie.contains(key).c(d!())
    }

    pub fn iter(&self) -> TrieIter<'_> {
        pnk!(self.trie.iter())
    }

    pub fn key_iter(&self) -> TrieKeyIter<'_> {
        pnk!(self.trie.key_iter())
    }

    pub fn root(&mut self) -> TrieRoot {
        *self.trie.root()
    }
}

struct MptMeta<'a> {
    // self-reference
    #[allow(dead_code)]
    cache: Option<LayeredCache<'a>>,

    // self-reference
    #[allow(dead_code)]
    root: *mut TrieRoot,
}

impl<'a> MptMeta<'a> {
    fn new(lc: Option<LocalTrieCache<H>>, root: TrieRoot, mutable: bool) -> Self {
        Self {
            cache: lc.map(|lc| LayeredCache::new(lc, alt!(mutable, None, Some(root)))),
            root: Box::into_raw(Box::new(root)),
        }
    }
}

// The raw pointers in `LayeredCache` will be dropped here
impl Drop for MptMeta<'_> {
    fn drop(&mut self) {
        unsafe {
            drop(Box::from_raw(self.root));
            if let Some(c) = mem::take(&mut self.cache) {
                Box::from_raw(c.cache).merge_into(&Box::from_raw(c.local_cache), *self.root);
            }
        }
    }
}

struct LayeredCache<'a> {
    // self-reference
    #[allow(dead_code)]
    cache: *mut TrieCache<'a, H>,

    // self-reference
    #[allow(dead_code)]
    local_cache: *mut LocalTrieCache<H>,
}

impl<'a> LayeredCache<'a> {
    fn new(lc: LocalTrieCache<H>, root: Option<TrieRoot>) -> Self {
        let lc = Box::into_raw(Box::new(lc));

        let cache = if let Some(root) = root {
            Box::into_raw(Box::new(unsafe { &*lc }.as_trie_db_cache(root)))
        } else {
            Box::into_raw(Box::new(unsafe { &*lc }.as_trie_db_mut_cache()))
        };

        Self {
            cache,
            local_cache: lc,
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn iter() {
        run(None);
    }

    #[test]
    fn iter_with_cache() {
        run(Some(1024))
    }

    fn run(cache_size: Option<usize>) {
        let s = MptStore::new();
        let mut hdr = pnk!(s.trie_create(b"backend_key", cache_size, false));

        {
            let samples = (0u8..200).map(|i| ([i], [i])).collect::<Vec<_>>();
            samples.iter().for_each(|(k, v)| {
                pnk!(hdr.insert(k, v));
            });

            let root = hdr.commit();

            let ro_hdr = hdr.ro_handle(root);
            let bt = ro_hdr
                .iter()
                .map(|i| i.unwrap())
                .collect::<BTreeMap<_, _>>();

            bt.iter().enumerate().for_each(|(i, (k, v))| {
                assert_eq!(&[i as u8], k.as_slice());
                assert_eq!(k, v);
            });

            let keylist = ro_hdr.key_iter().map(|i| i.unwrap()).collect::<Vec<_>>();
            assert_eq!(keylist, bt.keys().cloned().collect::<Vec<_>>());
        }

        {
            let samples = (0u8..200).map(|i| ([i], [i + 1])).collect::<Vec<_>>();
            samples.iter().for_each(|(k, v)| {
                pnk!(hdr.insert(k, v));
            });

            let root = hdr.commit();

            let ro_hdr = hdr.ro_handle(root);
            let bt = ro_hdr
                .iter()
                .map(|i| i.unwrap())
                .collect::<BTreeMap<_, _>>();

            bt.iter().enumerate().for_each(|(i, (k, v))| {
                assert_eq!(&[i as u8], k.as_slice());
                assert_eq!(&[k[0] + 1], v.as_slice());
            });

            let keylist = ro_hdr.key_iter().map(|i| i.unwrap()).collect::<Vec<_>>();
            assert_eq!(keylist, bt.keys().cloned().collect::<Vec<_>>());
        }

        assert!(!hdr.is_empty());
        hdr.clear().unwrap();
        assert!(hdr.is_empty());
    }
}
