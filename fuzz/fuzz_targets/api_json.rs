#![no_main]

use libfuzzer_sys::fuzz_target;
use minivmm::api::parse_command;
use minivmm::json::Json;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(value) = Json::parse(text) {
            let encoded = value.to_string();
            assert_eq!(Json::parse(&encoded).ok(), Some(value));
        }
        let _ = parse_command(text);
    }
});
