#![no_main]
use lattice_sql::{parse_statement, parse_statements};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    // Parsing never panics, an error points inside the input, and what parses prints back to text
    // that parses to the same tree.
    match parse_statements(text) {
        Ok(statements) => {
            for stmt in statements {
                let printed = stmt.to_string();
                assert_eq!(parse_statement(&printed).as_ref(), Ok(&stmt));
            }
        }
        Err(e) => {
            // The span lies inside the input and slices it on character boundaries.
            let span = e.span();
            assert!(span.start <= span.end && span.end <= text.len());
            assert!(text.get(span.start..span.end).is_some());
        }
    }
});
