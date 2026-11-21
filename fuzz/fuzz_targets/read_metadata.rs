#![no_main]
//! Metadata-only reading and its JSON rendering (which must parse).
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let l = mvdz::Limits {
        memory: 256 << 20,
        ..Default::default()
    };
    if let Ok(fi) = mvdz::read_metadata_with(std::io::Cursor::new(data), &l) {
        let j = fi.to_json();
        assert!(serde_json::from_str::<serde_json::Value>(&j).is_ok() || j.is_empty());
    }
});
