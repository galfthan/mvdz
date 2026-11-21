//! mvdz: lossless, format-aware compression of QuakeWorld MVD demos.
//!
//! One file format (FORMAT.md): a section container with metadata, a seek
//! index and a trailer, whose segments hold the demo
//! coded by a structural transform (static rANS at the normal level,
//! per-stream zstd at the fast level).
//!
//! ```no_run
//! let demo = std::fs::read("game.mvd").unwrap();
//! let packed = mvdz::compress(&demo, &mvdz::Config::default()).unwrap();
//! assert!(mvdz::is_mvdz(&packed));
//! assert_eq!(mvdz::decompress(&packed, 1).unwrap(), demo);
//! ```

pub mod api;
pub mod budget;
pub mod capi;
pub mod coder;
pub mod decode;
pub mod encode;
pub mod error;
pub mod format;
pub mod meta;
pub mod numeric;
pub mod proto;
#[cfg(all(feature = "sampler", target_os = "linux", target_arch = "x86_64"))]
pub mod sampler;
pub mod segment;
pub mod stream;
pub mod streams;
pub mod util;
pub mod walk;
pub mod zst;

pub use api::{
    Config, FileInfo, Info, Writer, check_demo, compress, compress_info, decompress,
    decompress_stream, decompress_with, gunzip, is_gzip, is_mvdz, read_metadata,
    read_metadata_with, write_file_atomic, write_file_atomic_with, write_gzip,
};
pub use budget::{Limits, ReadOptions};
pub use error::{Error, Result};
pub use format::{Index, IndexSeg, Metadata};
pub use segment::{DEFAULT_SEGMENT, Level};
pub use stream::{Reader, Seekable, Seq};
