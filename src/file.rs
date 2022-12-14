use core::mem::size_of;
use core::{fmt, mem};

use crate::alloc::Allocator;
use crate::config::*;
use crate::flash::Flash;
use crate::page::{ChunkHeader, Header, PageHeader, PageManager, PageReader, PageWriter, ReadError};
use crate::types::{OptionPageID, PageID};
use crate::{page, Error};

pub const PAGE_MAX_PAYLOAD_SIZE: usize = PAGE_SIZE - PageHeader::SIZE - size_of::<DataHeader>() - ChunkHeader::SIZE;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SeekDirection {
    Left,
    Right,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct MetaHeader {
    seq: Seq,
}

unsafe impl page::Header for MetaHeader {
    const MAGIC: u32 = 0x470b635c;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct DataHeader {
    seq: Seq,

    /// skiplist[0] = previous page, always.
    /// skiplist[i] = latest page that contains a byte with seq multiple of 2**(SKIPLIST_SHIFT+i)
    skiplist: [OptionPageID; SKIPLIST_LEN],

    /// Offset of the first record boundary within this page.
    /// u16::MAX if there's no boundary within the page. This can happen if a very big record
    /// starts at a previous page, and ends at a later page.
    record_boundary: u16,
}

unsafe impl page::Header for DataHeader {
    const MAGIC: u32 = 0xa934c056;
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct FileMeta {
    file_id: FileID,
    last_page_id: OptionPageID,
    first_seq: Seq,
}
impl_bytes!(FileMeta);

#[derive(Debug, Clone, Copy)]
struct FileState {
    last_page: Option<PagePointer>,
    first_seq: Seq,
    last_seq: Seq,
}

pub struct FileManager<F: Flash> {
    flash: F,
    pages: PageManager<F>,
    files: [FileState; FILE_COUNT],
    meta_page_id: PageID,
    meta_seq: Seq,

    alloc: Allocator,
}

impl<F: Flash> FileManager<F> {
    pub fn new(flash: F) -> Self {
        const DUMMY_FILE: FileState = FileState {
            last_page: None,
            first_seq: Seq::ZERO,
            last_seq: Seq::ZERO,
        };
        Self {
            flash,
            meta_page_id: PageID::from_raw(0).unwrap(),
            meta_seq: Seq::ZERO,
            pages: PageManager::new(),
            files: [DUMMY_FILE; FILE_COUNT],
            alloc: Allocator::new(),
        }
    }

    pub fn flash_mut(&mut self) -> &mut F {
        &mut self.flash
    }

    pub fn is_empty(&mut self, file_id: FileID) -> bool {
        self.files[file_id as usize].last_page.is_none()
    }

    pub fn read(&mut self, file_id: FileID) -> FileReader {
        FileReader::new(self, file_id)
    }

    pub fn write(&mut self, file_id: FileID) -> FileWriter {
        FileWriter::new(self, file_id)
    }

    pub fn format(&mut self) {
        // Erase all meta pages.
        for page_id in 0..PAGE_COUNT {
            let page_id = PageID::from_raw(page_id as _).unwrap();
            if let Ok(h) = self.read_header::<MetaHeader>(page_id) {
                self.flash.erase(page_id);
            }
        }

        // Write initial meta page.
        let page_id = PageID::from_raw(0).unwrap();
        let mut w = self.write_page(page_id);
        w.write_header(&mut self.flash, MetaHeader { seq: Seq(1) });
    }

    pub fn mount(&mut self) -> Result<(), Error> {
        self.alloc.reset();

        let mut meta_page_id = None;
        let mut meta_seq = Seq::ZERO;
        for page_id in 0..PAGE_COUNT {
            let page_id = PageID::from_raw(page_id as _).unwrap();
            if let Ok(h) = self.read_header::<MetaHeader>(page_id) {
                if h.seq > meta_seq {
                    meta_page_id = Some(page_id);
                    meta_seq = h.seq;
                }
            }
        }

        let Some(meta_page_id) = meta_page_id else {
            debug!("Meta page not found");
            return Err(Error::Corrupted)
        };
        self.meta_page_id = meta_page_id;
        self.meta_seq = meta_seq;
        self.alloc.mark_used(meta_page_id);

        #[derive(Clone, Copy)]
        struct FileInfo {
            last_page_id: OptionPageID,
            first_seq: Seq,
        }

        let (_, mut r) = self.read_page::<MetaHeader>(meta_page_id).inspect_err(|e| {
            debug!("failed read meta_page_id={:?}: {:?}", meta_page_id, e);
        })?;
        let mut files = [FileInfo {
            last_page_id: OptionPageID::none(),
            first_seq: Seq::ZERO,
        }; FILE_COUNT];
        loop {
            let mut meta = [0; FileMeta::SIZE];
            let n = r.read(&mut self.flash, &mut meta)?;
            if n == 0 {
                break;
            }
            if n != FileMeta::SIZE {
                debug!("meta page last entry incomplete, size {}", n);
                return Err(Error::Corrupted);
            }

            let meta = FileMeta::from_bytes(meta);
            if meta.file_id >= FILE_COUNT as _ {
                debug!("meta file_id out of range: {}", meta.file_id);
                return Err(Error::Corrupted);
            }

            if let Some(page_id) = meta.last_page_id.into_option() {
                if page_id.index() >= PAGE_COUNT {
                    debug!("meta last_page_id out of range: {}", meta.file_id);
                    return Err(Error::Corrupted);
                }
            } else {
                if meta.first_seq.0 != 0 {
                    debug!("meta last_page_id invalid, but first seq nonzero: {}", meta.file_id);
                    return Err(Error::Corrupted);
                }
            }

            files[meta.file_id as usize] = FileInfo {
                last_page_id: meta.last_page_id,
                first_seq: meta.first_seq,
            }
        }

        for file_id in 0..FILE_COUNT as FileID {
            let fi = files[file_id as usize];

            let Some(last_page_id) = fi.last_page_id.into_option() else {
                continue;
            };

            let (h, mut r) = self.read_page::<DataHeader>(last_page_id).inspect_err(|e| {
                debug!("read last_page_id={:?} file_id={}: {:?}", last_page_id, file_id, e);
            })?;
            let page_len = r.skip(&mut self.flash, PAGE_SIZE)?;
            let last_seq = h.seq.add(page_len)?;

            let mut p = Some(PagePointer {
                page_id: last_page_id,
                header: h,
            });

            // note: first_seq == last_seq is corruption too, because in that case what we do is delete the file.
            if fi.first_seq >= last_seq {
                debug!(
                    "meta: file {} first_seq {:?} not smaller than last_seq {:?}",
                    file_id, fi.first_seq, last_seq
                );
                return Err(Error::Corrupted);
            }

            self.files[file_id as usize] = FileState {
                last_page: p,
                first_seq: fi.first_seq,
                last_seq,
            };

            while let Some(pp) = p {
                if self.alloc.is_used(pp.page_id) {
                    info!("page used by multiple files at the same time. page_id={:?}", pp.page_id);
                    return Err(Error::Corrupted);
                }
                self.alloc.mark_used(pp.page_id);
                p = pp.prev(self, fi.first_seq)?;
            }
        }

        Ok(())
    }

    // TODO remove this
    pub fn rename(&mut self, from: FileID, to: FileID) -> Result<(), Error> {
        self.files.swap(from as usize, to as usize);
        self.commit_and_truncate(None, &[])
    }

    pub fn commit(&mut self, w: &mut FileWriter) -> Result<(), Error> {
        self.commit_and_truncate(Some(w), &[])
    }

    fn write_file_meta(&mut self, w: &mut PageWriter<MetaHeader>, file_id: FileID) -> Result<(), WriteError> {
        let f = &self.files[file_id as usize];
        let meta = FileMeta {
            file_id,
            first_seq: f.first_seq,
            last_page_id: f.last_page.as_ref().map(|pp| pp.page_id).into(),
        };
        let n = w.write(&mut self.flash, &meta.to_bytes());
        if n != FileMeta::SIZE {
            return Err(WriteError::Full);
        }

        Ok(())
    }

    pub fn commit_and_truncate(
        &mut self,
        mut w: Option<&mut FileWriter>,
        truncate: &[(FileID, usize)],
    ) -> Result<(), Error> {
        if let Some(w) = &mut w {
            w.do_commit(self)?;
        }

        for &(file_id, trunc_len) in truncate {
            let f = &mut self.files[file_id as usize];

            let len = f.last_seq.sub(f.first_seq);
            let seq = f.first_seq.add(len.min(trunc_len))?;

            let old_seq = f.first_seq;
            let p = if seq == f.last_seq {
                f.last_page
            } else {
                self.get_file_page(file_id, seq)?.unwrap().prev(self, old_seq)?
            };
            self.free_between(p, None, old_seq)?;

            let f = &mut self.files[file_id as usize];
            if seq == f.last_seq {
                f.first_seq = Seq::ZERO;
                f.last_seq = Seq::ZERO;
                f.last_page = None;
            } else {
                f.first_seq = seq;
            }
        }

        // Try appending to the existing meta page.
        let res: Result<(), WriteError> = try {
            let (_, mut mw) = self.pages.write_append(&mut self.flash, self.meta_page_id)?;
            if let Some(w) = w {
                self.write_file_meta(&mut mw, w.file_id)?;
            }
            for &(file_id, _) in truncate {
                self.write_file_meta(&mut mw, file_id)?;
            }
            mw.commit(&mut self.flash);
        };
        match res {
            Ok(()) => Ok(()),
            Err(WriteError::Corrupted) => Err(Error::Corrupted),
            Err(WriteError::Full) => {
                // Existing meta page was full. Write a new one.

                let page_id = self.alloc.allocate();
                let mut w = self.write_page(page_id);
                for file_id in 0..FILE_COUNT as FileID {
                    // Since we're writing a new page from scratch, no need to
                    // write metas for empty files.
                    if self.files[file_id as usize].last_page.is_some() {
                        self.write_file_meta(&mut w, file_id).unwrap();
                    }
                }

                self.meta_seq = self.meta_seq.add(1)?; // TODO handle wraparound
                w.write_header(&mut self.flash, MetaHeader { seq: self.meta_seq });
                w.commit(&mut self.flash);

                // free the old one.
                self.alloc.free(self.meta_page_id);
                self.meta_page_id = page_id;

                Ok(())
            }
        }
    }

    fn get_file_page(&mut self, file_id: FileID, seq: Seq) -> Result<Option<PagePointer>, Error> {
        let f = &self.files[file_id as usize];
        let Some(last) = f.last_page else {
            return Ok(None)
        };
        if seq < f.first_seq || seq >= f.last_seq {
            Ok(None)
        } else {
            Ok(Some(last.prev_seq(self, seq)?))
        }
    }

    fn free_between(
        &mut self,
        mut from: Option<PagePointer>,
        to: Option<PagePointer>,
        seq_limit: Seq,
    ) -> Result<(), Error> {
        while let Some(pp) = from {
            if let Some(to) = to {
                if pp.page_id == to.page_id {
                    break;
                }
            }
            self.alloc.free(pp.page_id);
            from = pp.prev(self, seq_limit)?;
        }
        Ok(())
    }

    fn read_page<H: Header>(&mut self, page_id: PageID) -> Result<(H, PageReader), Error> {
        self.pages.read(&mut self.flash, page_id)
    }

    fn read_header<H: Header>(&mut self, page_id: PageID) -> Result<H, Error> {
        self.pages.read_header(&mut self.flash, page_id)
    }

    fn write_page<H: Header>(&mut self, page_id: PageID) -> PageWriter<H> {
        self.pages.write(&mut self.flash, page_id)
    }

    #[cfg(feature = "std")]
    pub(crate) fn dump(&mut self) {
        for file_id in 0..FILE_COUNT {
            debug!("====== FILE {} ======", file_id);
            self.dump_file(file_id as _)
        }
    }

    #[cfg(feature = "std")]
    pub(crate) fn dump_file(&mut self, file_id: FileID) {
        let f = self.files[file_id as usize];
        debug!(
            "  seq: {:?}..{:?} len {:?} last_page {:?}",
            f.first_seq,
            f.last_seq,
            f.last_seq.sub(f.first_seq),
            f.last_page.map(|p| p.page_id)
        );

        let mut pages = Vec::new();
        let mut pp = f.last_page;
        while let Some(p) = pp {
            pages.push(p);
            pp = p.prev(self, f.first_seq).unwrap();
        }

        let mut buf = [0; 1024];
        for p in pages.iter().rev() {
            let (h, mut r) = self.read_page::<DataHeader>(p.page_id).unwrap();
            let n = r.read(&mut self.flash, &mut buf).unwrap();
            let data = &buf[..n];

            let rb = if h.record_boundary == u16::MAX {
                "None".to_string()
            } else {
                format!(
                    "{} (seq {:?})",
                    h.record_boundary,
                    h.seq.add(h.record_boundary as usize).unwrap()
                )
            };

            debug!(
                "  page {:?}: seq {:?}..{:?} len {:?} record_boundary {} skiplist {:?}",
                p.page_id,
                h.seq,
                h.seq.add(n).unwrap(),
                n,
                rb,
                h.skiplist
            );

            let mut s = h.seq;
            for c in data.chunks(32) {
                debug!("     {:04}: {:02x?}", s.0, c);
                s = s.add(c.len()).unwrap();
            }
        }
    }
}

/// "cursor" pointing to a page within a file.
#[derive(Clone, Copy, Debug)]
struct PagePointer {
    page_id: PageID,
    header: DataHeader,
}

impl PagePointer {
    fn prev(self, m: &mut FileManager<impl Flash>, seq_limit: Seq) -> Result<Option<PagePointer>, Error> {
        if self.header.seq <= seq_limit {
            return Ok(None);
        }

        let Some(p2) = self.header.skiplist[0].into_option() else {
            return Ok(None);
        };

        if p2.index() >= PAGE_COUNT {
            debug!("prev page out of range {:?}", p2);
            return Err(Error::Corrupted);
        }

        let h2 = m.read_header::<DataHeader>(p2)?;

        // Check seq always decreases. This avoids infinite loops
        // TODO we can make this check stricter: h2.seq == self.header.seq - page_len
        if h2.seq >= self.header.seq {
            debug!(
                "seq not decreasing when walking back: curr={:?} prev={:?}",
                self.header.seq, h2.seq
            );
            return Err(Error::Corrupted);
        }
        Ok(Some(PagePointer {
            page_id: p2,
            header: h2,
        }))
    }

    fn prev_seq(mut self, m: &mut FileManager<impl Flash>, seq: Seq) -> Result<PagePointer, Error> {
        while self.header.seq > seq {
            let i = skiplist_index(seq, self.header.seq);
            let Some(p2) = self.header.skiplist[i].into_option() else {
                debug!("no prev page??");
                return Err(Error::Corrupted);
            };
            if p2.index() >= PAGE_COUNT {
                debug!("prev page out of range {:?}", p2);
                return Err(Error::Corrupted);
            }

            let h2 = m.read_header::<DataHeader>(p2)?;

            // Check seq always decreases. This avoids infinite loops
            if h2.seq >= self.header.seq {
                debug!(
                    "seq not decreasing when walking back: curr={:?} prev={:?}",
                    self.header.seq, h2.seq
                );
                return Err(Error::Corrupted);
            }
            self = PagePointer {
                page_id: p2,
                header: h2,
            }
        }
        Ok(self)
    }
}

pub struct FileReader {
    file_id: FileID,
    state: ReaderState,
}

enum ReaderState {
    Created,
    Reading(ReaderStateReading),
    Finished,
}
struct ReaderStateReading {
    seq: Seq,
    reader: PageReader,
}

impl FileReader {
    fn new(_m: &mut FileManager<impl Flash>, file_id: FileID) -> Self {
        Self {
            file_id,
            state: ReaderState::Created,
        }
    }

    pub(crate) fn curr_seq(&mut self, m: &mut FileManager<impl Flash>) -> Seq {
        match &self.state {
            ReaderState::Created => m.files[self.file_id as usize].first_seq,
            ReaderState::Reading(s) => s.seq,
            ReaderState::Finished => unreachable!(),
        }
    }

    fn next_page(&mut self, m: &mut FileManager<impl Flash>) -> Result<(), Error> {
        let seq = self.curr_seq(m);
        self.seek_seq(m, seq)
    }

    fn seek_seq(&mut self, m: &mut FileManager<impl Flash>, seq: Seq) -> Result<(), Error> {
        self.state = match m.get_file_page(self.file_id, seq)? {
            Some(pp) => {
                let (h, mut r) = m.read_page::<DataHeader>(pp.page_id).inspect_err(|e| {
                    debug!("failed read next page={:?}: {:?}", pp.page_id, e);
                })?;
                let n = (seq.sub(h.seq)) as usize;
                let got_n = r.skip(&mut m.flash, n)?;
                let eof = r.is_at_eof(&mut m.flash)?;
                if n != got_n || eof {
                    debug!(
                        "found seq hole in file. page={:?} h.seq={:?} seq={:?} n={} got_n={} eof={}",
                        pp.page_id, h.seq, seq, n, got_n, eof
                    );
                    return Err(Error::Corrupted);
                }
                ReaderState::Reading(ReaderStateReading { seq, reader: r })
            }
            None => ReaderState::Finished,
        };
        Ok(())
    }

    pub fn read(&mut self, m: &mut FileManager<impl Flash>, mut data: &mut [u8]) -> Result<(), ReadError> {
        while !data.is_empty() {
            match &mut self.state {
                ReaderState::Finished => return Err(ReadError::Eof),
                ReaderState::Created => {
                    self.next_page(m)?;
                    continue;
                }
                ReaderState::Reading(s) => {
                    let n = s.reader.read(&mut m.flash, data)?;
                    data = &mut data[n..];
                    s.seq = s.seq.add(n)?;
                    if n == 0 {
                        self.next_page(m)?;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn skip(&mut self, m: &mut FileManager<impl Flash>, mut len: usize) -> Result<(), ReadError> {
        // advance within the current page.
        if let ReaderState::Reading(s) = &mut self.state {
            // Only worth trying if the skip might not exhaust the current page
            if len < PAGE_MAX_PAYLOAD_SIZE {
                let n = s.reader.skip(&mut m.flash, len)?;
                len -= n;
                s.seq = s.seq.add(n).unwrap();
            }
        }

        // If we got more to skip
        if len != 0 {
            let seq = match &self.state {
                ReaderState::Created => m.files[self.file_id as usize].first_seq,
                ReaderState::Reading(s) => s.seq,
                ReaderState::Finished => unreachable!(),
            };

            let new_seq = seq.add(len).map_err(|_| ReadError::Eof)?;
            if new_seq > m.files[self.file_id as usize].last_seq {
                return Err(ReadError::Eof);
            }

            self.seek_seq(m, new_seq)?;
        }

        Ok(())
    }

    pub fn offset(&mut self, m: &mut FileManager<impl Flash>) -> usize {
        let first_seq = m.files[self.file_id as usize].first_seq;
        self.curr_seq(m).sub(first_seq)
    }

    pub fn seek(&mut self, m: &mut FileManager<impl Flash>, offs: usize) -> Result<(), ReadError> {
        let first_seq = m.files[self.file_id as usize].first_seq;
        let new_seq = first_seq.add(offs).map_err(|_| ReadError::Eof)?;
        if new_seq > m.files[self.file_id as usize].last_seq {
            return Err(ReadError::Eof);
        }

        self.seek_seq(m, new_seq)?;
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SearchSeekError {
    TooMuchLeft,
    Corrupted,
}

impl From<Error> for SearchSeekError {
    fn from(e: Error) -> Self {
        match e {
            Error::Corrupted => Self::Corrupted,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum WriteError {
    Full,
    Corrupted,
}

impl From<Error> for WriteError {
    fn from(e: Error) -> Self {
        match e {
            Error::Corrupted => Self::Corrupted,
        }
    }
}

pub struct FileSearcher {
    r: FileReader,

    left: Seq,
    left_page: OptionPageID,

    right: Seq,
    right_skiplist: [OptionPageID; SKIPLIST_LEN],

    curr_low: Seq,
    curr_high: Seq,
    curr_page: OptionPageID,
    curr_skiplist: [OptionPageID; SKIPLIST_LEN],
}

impl FileSearcher {
    pub fn new(r: FileReader) -> Self {
        Self {
            r,
            left: Seq::MAX,
            left_page: OptionPageID::none(),
            right: Seq::MAX,
            right_skiplist: [OptionPageID::none(); SKIPLIST_LEN],
            curr_low: Seq::MAX,
            curr_high: Seq::MAX,
            curr_page: OptionPageID::none(),
            curr_skiplist: [OptionPageID::none(); SKIPLIST_LEN],
        }
    }

    pub fn start(&mut self, m: &mut FileManager<impl Flash>) -> Result<bool, Error> {
        let f = &m.files[self.r.file_id as usize];
        self.left = f.first_seq;
        self.right = f.last_seq;

        match f.last_page {
            Some(pp) => {
                if f.last_seq <= pp.header.seq {
                    return Err(Error::Corrupted);
                }

                // Create skiplist.
                self.right_skiplist = pp.header.skiplist;
                let top = skiplist_index(pp.header.seq, f.last_seq) + 1;
                self.right_skiplist[..top].fill(pp.page_id.into());

                trace!(
                    "search start file={:?}: left {:?} right {:?} right_skiplist {:?}",
                    self.r.file_id,
                    self.left,
                    self.right,
                    self.right_skiplist
                );
                self.really_seek(m)
            }
            None => {
                trace!("search start file={:?}: empty file", self.r.file_id);
                Ok(false)
            }
        }
    }

    pub fn reader(&mut self) -> &mut FileReader {
        &mut self.r
    }

    fn really_seek(&mut self, m: &mut FileManager<impl Flash>) -> Result<bool, Error> {
        let left = self.left.add(1).unwrap();
        let mut i = if left >= self.right {
            0
        } else {
            skiplist_index(left, self.right)
        };

        loop {
            if self.right > self.left {
                if let Some(page_id) = self.right_skiplist[i].into_option() {
                    let seq = skiplist_seq(self.right, i);
                    if seq >= self.left {
                        self.curr_high = seq;
                        match self.seek_to_page(m, page_id, Some(self.left)) {
                            Ok(()) => return Ok(true),
                            Err(SearchSeekError::Corrupted) => return Err(Error::Corrupted),
                            Err(SearchSeekError::TooMuchLeft) => {
                                let new_left = seq.add(1).unwrap();
                                assert!(new_left >= self.left);
                                self.left = new_left;
                            }
                        }
                    }
                }
            }

            if i == 0 {
                let page_id = self.left_page.into_option().unwrap();
                return match self.seek_to_page(m, page_id, None) {
                    Ok(()) => Ok(false),
                    Err(SearchSeekError::Corrupted) => Err(Error::Corrupted),
                    Err(SearchSeekError::TooMuchLeft) => unreachable!(),
                };
            }

            // Seek failed. Try again less hard.
            i -= 1;
        }
    }

    fn seek_to_page(
        &mut self,
        m: &mut FileManager<impl Flash>,
        mut page_id: PageID,
        left_limit: Option<Seq>,
    ) -> Result<(), SearchSeekError> {
        trace!("search: seek_to_page page_id={:?} left_limit={:?}", page_id, left_limit);

        let (h, mut r) = loop {
            if page_id.index() >= PAGE_COUNT {
                return Err(SearchSeekError::Corrupted);
            }
            let (h, r) = m.read_page::<DataHeader>(page_id).inspect_err(|e| {
                debug!("failed read next page={:?}: {:?}", page_id, e);
            })?;

            if h.skiplist[0].is_none() {
                trace!("search: arrived to first file page, setting left_page to {:?}", page_id);
                self.left_page = page_id.into();
            }

            if let Some(left_limit) = left_limit {
                if h.seq < left_limit {
                    trace!("search: seek_to_page hit left_limit");
                    return Err(SearchSeekError::TooMuchLeft);
                }
            }

            if h.record_boundary != u16::MAX {
                break (h, r);
            }

            // No record boundary within this page. Try the previous one.
            if let Some(prev) = h.skiplist[0].into_option() {
                page_id = prev;
                trace!("search: no record boundary, trying again with page {:?}", page_id);
            } else {
                debug!("first page in file has no record boundary!");
                return Err(SearchSeekError::Corrupted);
            }
        };

        // seek to record start.
        let b = h.record_boundary as usize;
        let n = r.skip(&mut m.flash, b)?;
        if n != b {
            return Err(SearchSeekError::Corrupted);
        }
        let state_seq = h.seq.add(b).unwrap();

        self.r.state = ReaderState::Reading(ReaderStateReading {
            seq: state_seq,
            reader: r,
        });
        self.curr_low = h.seq;
        self.curr_skiplist = h.skiplist;
        self.curr_page = page_id.into();
        trace!(
            "search: curr seq={:?}..{:?} page {:?} skiplist={:?}",
            self.curr_low,
            self.curr_high,
            self.curr_page,
            self.curr_skiplist
        );
        trace!("        left seq={:?} page {:?}", self.left, self.left_page,);
        trace!(
            "        right seq={:?}   skiplist={:?}",
            self.right,
            self.right_skiplist
        );

        Ok(())
    }

    pub fn seek(&mut self, m: &mut FileManager<impl Flash>, dir: SeekDirection) -> Result<bool, Error> {
        match dir {
            SeekDirection::Left => {
                trace!("search seek left");
                self.right = self.curr_low;
                self.right_skiplist = self.curr_skiplist;
            }
            SeekDirection::Right => {
                trace!("search seek right");
                self.left = self.curr_high.add(1).unwrap();
                self.left_page = self.curr_page;
            }
        }

        self.really_seek(m)
    }
}

pub struct FileWriter {
    file_id: FileID,
    last_page: Option<PagePointer>,
    seq: Seq,
    record_boundary: Option<usize>,
    writer: Option<PageWriter<DataHeader>>,
}

impl FileWriter {
    fn new(m: &mut FileManager<impl Flash>, file_id: FileID) -> Self {
        let f = &m.files[file_id as usize];

        Self {
            file_id,
            last_page: f.last_page,
            seq: f.last_seq,
            record_boundary: Some(0),
            writer: None,
        }
    }

    fn flush_header(&mut self, m: &mut FileManager<impl Flash>, mut w: PageWriter<DataHeader>) -> Result<(), Error> {
        let page_size = w.len();
        let page_id = w.page_id();
        let mut skiplist = [OptionPageID::none(); SKIPLIST_LEN];
        if let Some(last_page) = &self.last_page {
            skiplist = last_page.header.skiplist;

            let top = skiplist_index(last_page.header.seq, self.seq) + 1;
            skiplist[..top].fill(last_page.page_id.into());
        }

        let next_seq = self.seq.add(page_size)?;

        let record_boundary = match self.record_boundary {
            Some(b) if b >= page_size => u16::MAX,
            Some(b) => {
                self.record_boundary = None;
                b as u16
            }
            None => u16::MAX,
        };
        let header = DataHeader {
            seq: self.seq,
            skiplist,
            record_boundary,
        };
        w.write_header(&mut m.flash, header);
        w.commit(&mut m.flash);

        trace!(
            "flush_header: page={:?} h={:?} record_boundary={:?}",
            page_id,
            header,
            self.record_boundary
        );

        self.seq = next_seq;
        self.last_page = Some(PagePointer { page_id, header });

        Ok(())
    }

    fn next_page(&mut self, m: &mut FileManager<impl Flash>) -> Result<(), Error> {
        if let Some(w) = mem::replace(&mut self.writer, None) {
            self.flush_header(m, w)?;
        }

        let page_id = m.alloc.allocate();
        self.writer = Some(m.write_page(page_id));
        Ok(())
    }

    pub fn write(&mut self, m: &mut FileManager<impl Flash>, mut data: &[u8]) -> Result<(), Error> {
        while !data.is_empty() {
            match &mut self.writer {
                None => {
                    self.next_page(m)?;
                    continue;
                }
                Some(w) => {
                    let n = w.write(&mut m.flash, data);
                    data = &data[n..];
                    if n == 0 {
                        self.next_page(m)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn do_commit(&mut self, m: &mut FileManager<impl Flash>) -> Result<(), Error> {
        if let Some(w) = mem::replace(&mut self.writer, None) {
            self.flush_header(m, w)?;
            let f = &mut m.files[self.file_id as usize];
            f.last_page = self.last_page;
            f.last_seq = self.seq;
        }
        Ok(())
    }

    pub fn discard(&mut self, m: &mut FileManager<impl Flash>) {
        if let Some(w) = &self.writer {
            // Free the page we're writing now (not yet committed)
            let page_id = w.page_id();
            m.alloc.free(page_id);

            // Free previous pages, if any
            let f = &m.files[self.file_id as usize];
            m.free_between(self.last_page, f.last_page, Seq::ZERO).unwrap();
        };
    }

    pub fn record_end(&mut self) {
        if self.record_boundary.is_none() {
            self.record_boundary = Some(self.writer.as_mut().unwrap().len());
        }
    }
}

#[repr(transparent)]
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Seq(pub u32);

impl fmt::Display for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl fmt::Debug for Seq {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Seq {
    pub const ZERO: Self = Self(0);
    pub const MAX: Self = Self(u32::MAX);

    fn sub(self, other: Self) -> usize {
        self.0.checked_sub(other.0).unwrap() as _
    }

    fn add(self, offs: usize) -> Result<Self, Error> {
        let Ok(offs_u32) = offs.try_into() else {
            debug!("seq add overflow, offs doesn't fit u32: {}", offs);
            return Err(Error::Corrupted);
        };
        let Some(res) = self.0.checked_add(offs_u32) else {
            debug!("seq add overflow, self={} offs={}", self.0, offs_u32);
            return Err(Error::Corrupted);
        };
        Ok(Self(res))
    }
}

/// Find the highest amount of trailing zeros in all numbers in `a..b`.
///
/// Requires a < b.
/// Thanks @jannic for figuring out the beautiful bitwise hack!
fn max_trailing_zeros_between(a: u32, b: u32) -> u32 {
    assert!(a < b);

    if a == 0 {
        return 32;
    }

    31u32 - ((a - 1) ^ (b - 1)).leading_zeros()
}

/// Calculate skiplist index.
///
/// Requires `left < right`.
///
/// For building the skiplist: if the previous page has sequence numbers `left..right`,
/// what's the highest index that should be made to point to the previous page?
///
/// For iterating the skiplist: if we're at `right` and we want to seek backwards until
/// `left`, what's the highest index that we can use to jump back, without overshooting?
fn skiplist_index(left: Seq, right: Seq) -> usize {
    let bits = max_trailing_zeros_between(left.0, right.0) as usize;
    bits.saturating_sub(SKIPLIST_SHIFT).min(SKIPLIST_LEN - 1)
}

/// Calculate the destination seq of a skiplist jump.
///
/// Requires `curr` != 0.
///
/// If the current page starts at seq `curr`, and we jump backwards using the skiplist index
/// `index`, what's the sequence number the destination page is guaranteed to contain?
///
/// Note that the destination page will *contain* the returned seq, but it can *start*
/// earlier.
///
/// This is the inverse operation to `skiplist_index`, sort of.
fn skiplist_seq(curr: Seq, index: usize) -> Seq {
    let curr = curr.0.checked_sub(1).unwrap();
    let bits = match index {
        0 => 0,
        _ => index + SKIPLIST_SHIFT,
    };
    Seq(curr >> bits << bits)
}

#[cfg(test)]
mod tests {
    use rand::Rng;
    use test_log::test;

    use super::*;
    use crate::flash::MemFlash;
    use crate::types::RawPageID;

    fn page(p: RawPageID) -> PageID {
        PageID::from_raw(p).unwrap()
    }

    #[test]
    fn test_read_write() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let data = dummy_data(24);

        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        m.commit(&mut w).unwrap();

        let mut r = m.read(0);
        let mut buf = vec![0; data.len()];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(data, buf);

        // Remount
        m.mount().unwrap();

        let mut r = m.read(0);
        let mut buf = vec![0; data.len()];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(data, buf);
    }

    #[test]
    fn test_read_write_long() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let data = dummy_data(23456);

        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        m.commit(&mut w).unwrap();
        w.discard(&mut m);

        let mut r = m.read(0);
        let mut buf = vec![0; data.len()];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(data, buf);

        // Remount
        m.mount().unwrap();

        let mut r = m.read(0);
        let mut buf = vec![0; data.len()];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(data, buf);
    }

    #[test]
    fn test_append() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[1, 2, 3, 4, 5]).unwrap();
        m.commit(&mut w).unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[6, 7, 8, 9]).unwrap();
        m.commit(&mut w).unwrap();

        let mut r = m.read(0);
        let mut buf = vec![0; 9];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5, 6, 7, 8, 9]);

        // Remount
        m.mount().unwrap();

        let mut r = m.read(0);
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn test_truncate() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[1, 2, 3, 4, 5]).unwrap();
        m.commit(&mut w).unwrap();

        m.commit_and_truncate(None, &[(0, 2)]).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 3];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [3, 4, 5]);

        m.commit_and_truncate(None, &[(0, 1)]).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 2];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [4, 5]);

        // Remount
        m.mount().unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 2];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [4, 5]);
    }

    #[test]
    fn test_append_truncate() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[1, 2, 3, 4, 5]).unwrap();
        m.commit(&mut w).unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[6, 7, 8, 9]).unwrap();

        m.commit_and_truncate(Some(&mut w), &[(0, 2)]).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 7];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [3, 4, 5, 6, 7, 8, 9]);

        m.commit_and_truncate(None, &[(0, 1)]).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 6];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [4, 5, 6, 7, 8, 9]);

        m.commit_and_truncate(None, &[(0, 5)]).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 1];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [9]);

        // Remount
        m.mount().unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 1];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [9]);

        let mut w = m.write(0);
        w.write(&mut m, &[10, 11, 12]).unwrap();
        m.commit(&mut w).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 4];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [9, 10, 11, 12]);
    }

    #[test]
    fn test_delete() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[1, 2, 3, 4, 5]).unwrap();
        m.commit(&mut w).unwrap();

        assert_eq!(m.alloc.is_used(page(1)), true);

        m.commit_and_truncate(None, &[(0, 5)]).unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);

        // Remount
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);

        let mut r = m.read(0);
        let mut buf = [0; 1];
        let res = r.read(&mut m, &mut buf);
        assert!(matches!(res, Err(ReadError::Eof)));
    }

    #[test]
    fn test_truncate_alloc() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);

        let mut w = m.write(0);
        w.write(&mut m, &[1, 2, 3, 4, 5]).unwrap();
        m.commit(&mut w).unwrap();

        assert_eq!(m.alloc.is_used(page(1)), true);

        m.commit_and_truncate(None, &[(0, 5)]).unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);

        // Remount
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);

        let mut r = m.read(0);
        let mut buf = [0; 1];
        let res = r.read(&mut m, &mut buf);
        assert!(matches!(res, Err(ReadError::Eof)));
    }

    #[test]
    fn test_truncate_alloc_2() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);

        let data = dummy_data(PAGE_MAX_PAYLOAD_SIZE);

        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        w.write(&mut m, &data).unwrap();
        m.commit(&mut w).unwrap();

        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);

        m.commit_and_truncate(None, &[(0, PAGE_MAX_PAYLOAD_SIZE)]).unwrap();
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), true);

        m.commit_and_truncate(None, &[(0, PAGE_MAX_PAYLOAD_SIZE)]).unwrap();
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);

        // Remount
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);

        let mut r = m.read(0);
        let mut buf = [0; 1];
        let res = r.read(&mut m, &mut buf);
        assert!(matches!(res, Err(ReadError::Eof)));
    }

    #[test]
    fn test_append_no_commit() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[1, 2, 3, 4, 5]).unwrap();
        m.commit(&mut w).unwrap();

        let mut w = m.write(0);
        w.write(&mut m, &[6, 7, 8, 9]).unwrap();
        w.discard(&mut m);

        let mut r = m.read(0);
        let mut buf = [0; 5];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5]);
        let mut buf = [0; 1];
        let res = r.read(&mut m, &mut buf);
        assert!(matches!(res, Err(ReadError::Eof)));

        let mut w = m.write(0);
        w.write(&mut m, &[10, 11]).unwrap();
        m.commit(&mut w).unwrap();

        let mut r = m.read(0);
        let mut buf = [0; 7];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf, [1, 2, 3, 4, 5, 10, 11]);
    }

    #[test]
    fn test_read_unwritten() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut r = m.read(0);
        let mut buf = vec![0; 1024];
        let res = r.read(&mut m, &mut buf);
        assert!(matches!(res, Err(ReadError::Eof)));
    }

    #[test]
    fn test_read_uncommitted() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let data = dummy_data(1234);

        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        w.discard(&mut m); // don't commit

        let mut r = m.read(0);
        let mut buf = vec![0; 1024];
        let res = r.read(&mut m, &mut buf);
        assert!(matches!(res, Err(ReadError::Eof)));
    }

    #[test]
    fn test_alloc_commit() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);

        let data = dummy_data(PAGE_MAX_PAYLOAD_SIZE);
        let mut w = m.write(0);

        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true); // old meta
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);

        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true); // old meta
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);
        assert_eq!(m.alloc.is_used(page(3)), false);

        m.commit(&mut w).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true); // old meta, appended in-place.
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);
        assert_eq!(m.alloc.is_used(page(3)), false);

        // Remount
        m.mount().unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true); // old meta, appended in-place.
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);
        assert_eq!(m.alloc.is_used(page(3)), false);
    }

    #[test]
    fn test_alloc_discard_1page() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);

        let data = dummy_data(PAGE_MAX_PAYLOAD_SIZE);
        let mut w = m.write(0);

        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), false);

        w.discard(&mut m);
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);

        // Remount
        m.mount().unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);
    }

    #[test]
    fn test_alloc_discard_2page() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);

        let data = dummy_data(PAGE_MAX_PAYLOAD_SIZE);
        let mut w = m.write(0);

        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), false);

        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);
        assert_eq!(m.alloc.is_used(page(3)), false);

        w.discard(&mut m);
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);

        // Remount
        m.mount().unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);
    }

    #[test]
    fn test_alloc_discard_3page() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);

        let data = dummy_data(PAGE_MAX_PAYLOAD_SIZE * 3);
        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);
        assert_eq!(m.alloc.is_used(page(3)), true);
        assert_eq!(m.alloc.is_used(page(4)), false);

        w.discard(&mut m);
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);
        assert_eq!(m.alloc.is_used(page(4)), false);

        // Remount
        m.mount().unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);
        assert_eq!(m.alloc.is_used(page(4)), false);
    }

    #[test]
    fn test_append_alloc_discard() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), false);

        let data = dummy_data(24);
        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        m.commit(&mut w).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true); // old meta, appended in-place.
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);

        let mut w = m.write(0);
        w.write(&mut m, &data).unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), true);
        assert_eq!(m.alloc.is_used(page(3)), false);

        w.discard(&mut m);
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);

        // Remount
        m.mount().unwrap();
        assert_eq!(m.alloc.is_used(page(0)), true);
        assert_eq!(m.alloc.is_used(page(1)), true);
        assert_eq!(m.alloc.is_used(page(2)), false);
        assert_eq!(m.alloc.is_used(page(3)), false);
    }

    #[test]
    fn test_write_2_files() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let data = dummy_data(32);

        let mut w1 = m.write(1);
        w1.write(&mut m, &data).unwrap();

        let mut w2 = m.write(2);
        w2.write(&mut m, &data).unwrap();

        m.commit(&mut w2).unwrap();
        m.commit(&mut w1).unwrap();

        let mut r = m.read(1);
        let mut buf = [0; 32];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf[..], data[..]);

        let mut r = m.read(2);
        let mut buf = [0; 32];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf[..], data[..]);

        // Remount
        m.mount().unwrap();

        let mut r = m.read(1);
        let mut buf = [0; 32];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf[..], data[..]);

        let mut r = m.read(2);
        let mut buf = [0; 32];
        r.read(&mut m, &mut buf).unwrap();
        assert_eq!(buf[..], data[..]);
    }

    #[test]
    fn test_record_boundary_one() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let h = m.read_header::<DataHeader>(page(1)).unwrap();
        assert_eq!(h.seq, Seq(0));
        assert_eq!(h.record_boundary, 0);
    }

    #[test]
    fn test_record_boundary_two() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE + 1]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let h = m.read_header::<DataHeader>(page(1)).unwrap();
        assert_eq!(h.seq, Seq(0));
        assert_eq!(h.record_boundary, 0);

        let h = m.read_header::<DataHeader>(page(2)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32));
        assert_eq!(h.record_boundary, u16::MAX);
    }

    #[test]
    fn test_record_boundary_three() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE + 1]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let h = m.read_header::<DataHeader>(page(1)).unwrap();
        assert_eq!(h.seq, Seq(0));
        assert_eq!(h.record_boundary, 0);

        let h = m.read_header::<DataHeader>(page(2)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32));
        assert_eq!(h.record_boundary, 2);
    }

    #[test]
    fn test_record_boundary_overlong() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE * 2 + 1]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let h = m.read_header::<DataHeader>(page(1)).unwrap();
        assert_eq!(h.seq, Seq(0));
        assert_eq!(h.record_boundary, 0);

        let h = m.read_header::<DataHeader>(page(2)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32));
        assert_eq!(h.record_boundary, u16::MAX);

        let h = m.read_header::<DataHeader>(page(3)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 2));
        assert_eq!(h.record_boundary, u16::MAX);
    }

    #[test]
    fn test_record_boundary_overlong_2() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE * 2 + 1]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE * 2 + 1]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let h = m.read_header::<DataHeader>(page(1)).unwrap();
        assert_eq!(h.seq, Seq(0));
        assert_eq!(h.record_boundary, 0);

        let h = m.read_header::<DataHeader>(page(2)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32));
        assert_eq!(h.record_boundary, u16::MAX);

        let h = m.read_header::<DataHeader>(page(3)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 2));
        assert_eq!(h.record_boundary, 2);

        let h = m.read_header::<DataHeader>(page(4)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 3));
        assert_eq!(h.record_boundary, u16::MAX);

        let h = m.read_header::<DataHeader>(page(5)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 4));
        assert_eq!(h.record_boundary, u16::MAX);
    }

    #[test]
    fn test_record_boundary_overlong_3() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE * 2 + 1]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; PAGE_MAX_PAYLOAD_SIZE * 2 + 1]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00]).unwrap();
        w.record_end();

        m.commit(&mut w).unwrap();

        let h = m.read_header::<DataHeader>(page(1)).unwrap();
        assert_eq!(h.seq, Seq(0));
        assert_eq!(h.record_boundary, 0);

        let h = m.read_header::<DataHeader>(page(2)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32));
        assert_eq!(h.record_boundary, u16::MAX);

        let h = m.read_header::<DataHeader>(page(3)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 2));
        assert_eq!(h.record_boundary, 2);

        let h = m.read_header::<DataHeader>(page(4)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 3));
        assert_eq!(h.record_boundary, u16::MAX);

        let h = m.read_header::<DataHeader>(page(5)).unwrap();
        assert_eq!(h.seq, Seq(PAGE_MAX_PAYLOAD_SIZE as u32 * 4));
        assert_eq!(h.record_boundary, 3);
    }

    #[test]
    fn test_search_empty() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);
    }

    #[test]
    fn test_search_one_page() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00]).unwrap();
        m.commit(&mut w).unwrap();

        let mut buf = [0u8; 1];
        // Seek left
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        // Seek right
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);
    }

    #[test]
    fn test_search_two_pages() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        for _ in 0..2 {
            let mut w = m.write(1);
            w.write(&mut m, &[0x00]).unwrap();
            w.record_end();
            m.commit(&mut w).unwrap();
        }

        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 1);
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 1);
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 1);
    }

    #[test]
    fn test_search_no_boundary_on_second_page() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00; 238]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let mut buf = [0u8; 1];
        // Seek left
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        // Seek right
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);
    }

    #[test]
    fn test_search_no_boundary_more_pages() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00; 4348]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let mut buf = [0u8; 1];
        // Seek left
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        // Seek right
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);
    }

    #[test]
    fn test_search_no_boundary_more_pages_two() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00; 4348]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; 4348]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let mut buf = [0u8; 1];
        // Seek left
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 4348);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        // Seek right
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 4348);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 4348);
    }

    #[test]
    fn test_search_no_boundary_more_pages_three() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut w = m.write(1);
        w.write(&mut m, &[0x00; 4348]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; 4348]).unwrap();
        w.record_end();
        w.write(&mut m, &[0x00; 4348]).unwrap();
        w.record_end();
        m.commit(&mut w).unwrap();

        let mut buf = [0u8; 1];
        // Seek left
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 8696);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 4348);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        // Seek less left
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 8696);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 4348);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 0);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 0);

        // Seek middle
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 8696);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Left).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 4348);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 4348);

        // Seek right
        let mut s = FileSearcher::new(m.read(1));
        assert_eq!(s.start(&mut m).unwrap(), true);
        assert_eq!(s.reader().offset(&mut m), 8696);
        s.reader().read(&mut m, &mut buf).unwrap();
        assert_eq!(s.seek(&mut m, SeekDirection::Right).unwrap(), false);
        assert_eq!(s.reader().offset(&mut m), 8696);
    }

    #[test]
    fn test_search_long() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let count = 20000 / 4;

        let mut w = m.write(1);
        for i in 1..=count {
            w.write(&mut m, &(i as u32).to_le_bytes()).unwrap();
            // make records not line up with page boundaries.
            w.write(&mut m, &[0x00]).unwrap();
            w.record_end();
        }
        m.commit(&mut w).unwrap();

        for want in 0..count + 1 {
            debug!("searching for {}", want);

            let mut s = FileSearcher::new(m.read(1));
            assert_eq!(s.start(&mut m).unwrap(), true);

            loop {
                let mut record = [0; 4];
                s.reader().read(&mut m, &mut record).unwrap();
                let record = u32::from_le_bytes(record);

                let dir = if record >= want {
                    SeekDirection::Left
                } else {
                    SeekDirection::Right
                };
                let ok = s.seek(&mut m, dir).unwrap();
                if !ok {
                    break;
                }
            }

            let mut record = [0; 4];
            s.reader().read(&mut m, &mut record).unwrap();
            let record = u32::from_le_bytes(record);

            if want != 0 {
                assert!(want >= record);
                assert!(want - record <= PAGE_MAX_PAYLOAD_SIZE as u32 / 4);
            }
        }
    }

    fn dummy_data(len: usize) -> Vec<u8> {
        let mut res = vec![0; len];
        for (i, v) in res.iter_mut().enumerate() {
            *v = i as u8 ^ (i >> 8) as u8 ^ (i >> 16) as u8 ^ (i >> 24) as u8;
        }
        res
    }

    fn rand(max: usize) -> usize {
        rand::thread_rng().gen_range(0..max)
    }

    fn rand_between(from: usize, to: usize) -> usize {
        rand::thread_rng().gen_range(from..=to)
    }

    #[test]
    fn test_smoke() {
        let mut f = MemFlash::new();
        let mut m = FileManager::new(&mut f);
        m.format();
        m.mount().unwrap();

        let mut seq_min = 0;
        let mut seq_max = 0;
        let file_id = 0;

        let max_size = 2000;

        for _ in 0..100000 {
            match rand(20) {
                // truncate
                0 => {
                    let s = rand_between(0, seq_max - seq_min);
                    debug!("{} {}, truncate {}", seq_min, seq_max, s);
                    m.commit_and_truncate(None, &[(file_id, s)]).unwrap();
                    seq_min += s;
                }
                // append
                _ => {
                    let n = rand(1024);
                    if seq_max - seq_min + n > max_size {
                        continue;
                    }

                    debug!("{} {}, append {}", seq_min, seq_max, n);

                    let data: Vec<_> = (0..n).map(|i| (seq_max + i) as u8).collect();

                    let mut w = m.write(file_id);
                    w.write(&mut m, &data).unwrap();
                    m.commit(&mut w).unwrap();

                    seq_max += n;
                }
            }

            // ============ Check read all
            debug!("{} {}, read_all", seq_min, seq_max);

            let mut r = m.read(file_id);
            let mut data = vec![0; seq_max - seq_min];
            r.read(&mut m, &mut data).unwrap();
            let want_data: Vec<_> = (0..seq_max - seq_min).map(|i| (seq_min + i) as u8).collect();
            assert_eq!(data, want_data);

            // Check we're at EOF
            let mut buf = [0; 1];
            assert_eq!(r.read(&mut m, &mut buf), Err(ReadError::Eof));

            // ============ Check read seek
            let s = rand_between(0, seq_max - seq_min);
            debug!("{} {}, read_seek {}", seq_min, seq_max, s);

            let mut r = m.read(file_id);
            r.seek(&mut m, s).unwrap();
            let mut data = vec![0; seq_max - seq_min - s];
            r.read(&mut m, &mut data).unwrap();
            let want_data: Vec<_> = (0..seq_max - seq_min - s).map(|i| (seq_min + s + i) as u8).collect();
            assert_eq!(data, want_data);

            // Check we're at EOF
            let mut buf = [0; 1];
            assert_eq!(r.read(&mut m, &mut buf), Err(ReadError::Eof));

            // ============ Check read skip
            let s1 = rand_between(0, seq_max - seq_min);
            let s2 = rand_between(0, seq_max - seq_min - s1);
            debug!("{} {}, read_skip {} {}", seq_min, seq_max, s1, s2);

            let mut r = m.read(file_id);
            r.skip(&mut m, s1).unwrap();
            r.skip(&mut m, s2).unwrap();
            let mut data = vec![0; seq_max - seq_min - s1 - s2];
            r.read(&mut m, &mut data).unwrap();
            let want_data: Vec<_> = (0..seq_max - seq_min - s1 - s2)
                .map(|i| (seq_min + s1 + s2 + i) as u8)
                .collect();
            assert_eq!(data, want_data);

            // Check we're at EOF
            let mut buf = [0; 1];
            assert_eq!(r.read(&mut m, &mut buf), Err(ReadError::Eof));
        }
    }

    #[test]
    fn test_max_trailing_zeros_between() {
        fn slow(a: u32, b: u32) -> u32 {
            assert!(a < b);
            (a..b).map(|x| x.trailing_zeros()).max().unwrap()
        }

        fn meh(a: u32, b: u32) -> u32 {
            assert!(a < b);

            if a == 0 {
                return 32;
            }

            for i in (0..32).rev() {
                let x = (b - 1) >> i << i;
                if x >= a && x < b {
                    return i;
                }
            }
            0
        }

        // Test slow matches meh and fast.
        for a in 0..100 {
            for b in a + 1..100 {
                assert_eq!(slow(a, b), max_trailing_zeros_between(a, b), "failed at {} {}", a, b);
                assert_eq!(slow(a, b), meh(a, b), "failed at {} {}", a, b);
            }
        }

        // Test meh matches fast, for bigger numbers.
        let interesting = [
            0x0000_0000,
            0x0000_0001,
            0x0000_0002,
            0x3FFF_FFFE,
            0x3FFF_FFFF,
            0x4000_0000,
            0x4000_0001,
            0x7FFF_FFFE,
            0x7FFF_FFFF,
            0x8000_0000,
            0x8000_0001,
            0xBFFF_FFFE,
            0xBFFF_FFFF,
            0xC000_0000,
            0xC000_0001,
            0xFFFF_FFFE,
            0x7FFF_FFFF,
        ];

        for a in interesting {
            for b in interesting {
                if a < b {
                    assert_eq!(meh(a, b), max_trailing_zeros_between(a, b), "failed at {} {}", a, b);
                }
            }
        }
    }
}
