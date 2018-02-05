// Copyright 2016 `multipart` Crate Developers
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.
//! The server-side abstraction for multipart requests. Enabled with the `server` feature.
//!
//! Use this when you are implementing an HTTP server and want to
//! to accept, parse, and serve HTTP `multipart/form-data` requests (file uploads).
//!
//! See the `Multipart` struct for more info.

pub extern crate buf_redux;
extern crate httparse;
extern crate twoway;

use std::borrow::Borrow;
use std::io::prelude::*;
use std::path::Path;
use std::sync::Arc;
use std::io;

use tempdir::TempDir;

use self::boundary::BoundaryReader;

use self::field::PrivReadEntry;

pub use self::field::{FieldHeaders, MultipartField, MultipartData, ReadEntry, ReadEntryResult};

use self::save::SaveBuilder;

pub use self::save::{Entries, SaveResult, SavedField};

use self::save::EntriesSaveResult;

/// Default typedef for shared strings.
///
/// Enable the `use_arc_str` feature to use `Arc<str>` instead, which saves an indirection but
/// cannot be constructed in Rust versions older than 1.21 (the `From<String>` impl was stabilized
/// in that release).
#[cfg(not(feature = "use_arc_str"))]
pub type ArcStr = Arc<String>;

/// Optimized typedef for shared strings, replacing `Arc<String>`.
///
/// Enabled with the `use_arc_str` feature.
#[cfg(feature = "use_arc_str")]
pub type ArcStr = Arc<str>;

macro_rules! try_opt (
    ($expr:expr) => (
        match $expr {
            Some(val) => val,
            None => return None,
        }
    );
    ($expr:expr, $before_ret:expr) => (
        match $expr {
            Some(val) => val,
            None => {
                $before_ret;
                return None;
            }
        }
    )
);

macro_rules! try_read_entry {
    ($self_:expr; $try:expr) => (
        match $try {
            Ok(res) => res,
            Err(err) => return ::server::ReadEntryResult::Error($self_, err),
        }
    )
}

mod boundary;
mod field;

#[cfg(feature = "hyper")]
pub mod hyper;

#[cfg(feature = "iron")]
pub mod iron;

#[cfg(feature = "tiny_http")]
pub mod tiny_http;

#[cfg(feature = "nickel")]
pub mod nickel;

pub mod save;

/// The server-side implementation of `multipart/form-data` requests.
///
/// Implements `Borrow<R>` to allow access to the request body, if desired.
pub struct Multipart<R> {
    reader: BoundaryReader<R>,
}

impl Multipart<()> {
    /// If the given `HttpRequest` is a multipart/form-data POST request,
    /// return the request body wrapped in the multipart reader. Otherwise,
    /// returns the original request.
    pub fn from_request<R: HttpRequest>(req: R) -> Result<Multipart<R::Body>, R> {
        //FIXME: move `map` expr to `Some` arm when nonlexical borrow scopes land.
        let boundary = match req.multipart_boundary().map(String::from) {
            Some(boundary) => boundary,
            None => return Err(req),
        };

        Ok(Multipart::with_body(req.body(), boundary))        
    }   
}

impl<R: Read> Multipart<R> {
    /// Construct a new `Multipart` with the given body reader and boundary.
    ///
    /// ## Note: `boundary`
    /// This will prepend the requisite `--` to the boundary string as documented in
    /// [IETF RFC 1341, Section 7.2.1: "Multipart: the common syntax"][rfc1341-7.2.1].
    /// Simply pass the value of the `boundary` key from the `Content-Type` header in the
    /// request (or use `Multipart::from_request()`, if supported).
    ///
    /// [rfc1341-7.2.1]: https://tools.ietf.org/html/rfc1341#page-30
    pub fn with_body<Bnd: Into<String>>(body: R, boundary: Bnd) -> Self {
        let boundary = boundary.into();

        info!("Multipart::with_boundary(_, {:?}", boundary);

        Multipart { 
            reader: BoundaryReader::from_reader(body, boundary),
        }
    }

    /// Read the next entry from this multipart request, returning a struct with the field's name and
    /// data. See `MultipartField` for more info.
    ///
    /// ## Warning: Risk of Data Loss
    /// If the previously returned entry had contents of type `MultipartField::File`,
    /// calling this again will discard any unread contents of that entry.
    pub fn read_entry(&mut self) -> io::Result<Option<MultipartField<&mut Self>>> {
        self.read_entry_mut().into_result()
    }

    /// Read the next entry from this multipart request, returning a struct with the field's name and
    /// data. See `MultipartField` for more info.
    pub fn into_entry(self) -> ReadEntryResult<Self> {
        self.read_entry()
    }

    /// Call `f` for each entry in the multipart request.
    /// 
    /// This is a substitute for Rust not supporting streaming iterators (where the return value
    /// from `next()` borrows the iterator for a bound lifetime).
    ///
    /// Returns `Ok(())` when all fields have been read, or the first error.
    pub fn foreach_entry<F>(&mut self, mut foreach: F) -> io::Result<()> where F: FnMut(MultipartField<&mut Self>) {
        loop {
            match self.read_entry() {
                Ok(Some(field)) => foreach(field),
                Ok(None) => return Ok(()),
                Err(err) => return Err(err),
            }
        }
    }

    /// Get a builder type for saving the files in this request to the filesystem.
    ///
    /// See [`SaveBuilder`](save/struct.SaveBuilder.html) for more information.
    pub fn save(&mut self) -> SaveBuilder<&mut Self> {
        SaveBuilder::new(self)
    }
}

impl<R> Borrow<R> for Multipart<R> {
    fn borrow(&self) -> &R {
        self.reader.borrow()
    }
}

impl<R: Read> PrivReadEntry for Multipart<R> {
    type Source = BoundaryReader<R>;

    fn source_mut(&mut self) -> &mut BoundaryReader<R> {
        &mut self.reader
    }

    fn set_min_buf_size(&mut self, min_buf_size: usize) {
        self.reader.set_min_buf_size(min_buf_size)
    }

    /// Consume the next boundary.
    /// Returns `true` if the last boundary was read, `false` otherwise.
    fn consume_boundary(&mut self) -> io::Result<bool> {
        debug!("Consume boundary!");
        self.reader.consume_boundary()
    }
}

/// A server-side HTTP request that may or may not be multipart.
///
/// May be implemented by mutable references if providing the request or body by-value is
/// undesirable.
pub trait HttpRequest {
    /// The body of this request.
    type Body: Read;
    /// Get the boundary string of this request if it is a POST request
    /// with the `Content-Type` header set to `multipart/form-data`.
    ///
    /// The boundary string should be supplied as an extra value of the `Content-Type` header, e.g.
    /// `Content-Type: multipart/form-data; boundary={boundary}`.
    fn multipart_boundary(&self) -> Option<&str>;

    /// Return the request body for reading.
    fn body(self) -> Self::Body;
}
